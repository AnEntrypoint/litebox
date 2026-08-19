// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! A [LiteBox platform](../litebox/platform/index.html) for running LiteBox on userland Linux.

// Restrict this crate to only work on Linux, on the architectures with a real implementation
// below (x86_64 and aarch64).
#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use std::cell::Cell;
use std::io::IsTerminal as _;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;
use std::unimplemented;

use litebox::fs::OFlags;
use litebox::platform::UnblockedOrTimedOut;
use litebox::platform::page_mgmt::{
    CowAllocationError, FixedAddressBehavior, MemoryRegionPermissions, SharedMemoryError,
};
use litebox::platform::{ImmediatelyWokenUp, RawConstPointer as _};
use litebox::shim::ContinueOperation;
use litebox::utils::{ReinterpretSignedExt, ReinterpretUnsignedExt as _, TruncateExt};
use litebox_common_linux::{MRemapFlags, MapFlags, ProtFlags, vmap::VmapManager};

use zerocopy::{FromBytes, IntoBytes};

extern crate alloc;

// ---------------------------------------------------------------------------
// TLS (`.tbss`) access helpers
//
// On x86_64, the ELF TLS model uses `@tpoff`; on x86 it uses `@ntpoff`.
// At guest-host transitions we swap `fs` and `gs`, so after the swap the host TLS base
// is in the normal segment register. Before the swap (e.g. in a signal
// handler that fires while the guest is running), the host TLS base is
// in the *saved* segment register (`gs` on x86_64, `fs` on x86).
//
// The macros below produce string literals so they can be used inside
// `concat!()` within `core::arch::asm!()`.
// ---------------------------------------------------------------------------

/// TLS relocation suffix: `"@tpoff"` on x86_64, `"@ntpoff"` on x86.
#[cfg(target_arch = "x86_64")]
macro_rules! tls_suffix {
    () => {
        "@tpoff"
    };
}

/// Segment register used for TLS after the fs/gs swap (normal host context).
#[cfg(target_arch = "x86_64")]
macro_rules! tls_seg {
    () => {
        "fs"
    };
}

/// Segment register where the host TLS base is saved before the swap
/// (signal handler context while the guest is running).
#[cfg(target_arch = "x86_64")]
macro_rules! saved_tls_seg {
    () => {
        "gs"
    };
}

/// Full TLS memory operand for a `.tbss` variable in normal host context
/// (after the fs/gs swap).
///
/// Example: `tls!("pending_host_signals")` expands to
/// `"fs:pending_host_signals@tpoff"` on x86_64.
macro_rules! tls {
    ($var:literal) => {
        concat!(tls_seg!(), ":", $var, tls_suffix!())
    };
}

/// Full TLS memory operand for a `.tbss` variable accessed via the *saved*
/// segment register (before the fs/gs swap, e.g. from a signal handler).
///
/// Example: `saved_tls!("in_guest")` expands to
/// `"gs:in_guest@tpoff"` on x86_64.
macro_rules! saved_tls {
    ($var:literal) => {
        concat!(saved_tls_seg!(), ":", $var, tls_suffix!())
    };
}

/// The userland Linux platform.
///
/// This implements the main [`litebox::platform::Provider`] trait, i.e., implements all platform
/// traits.
pub struct LinuxUserland {
    tun_socket_fd: std::sync::RwLock<Option<std::os::fd::OwnedFd>>,
    /// Reserved pages that are not available for guest programs to use.
    reserved_pages: Vec<core::ops::Range<usize>>,
    /// CoW-eligible memory regions. Maps start address of the static slice, to the info needed to
    /// re-mmap the file.
    cow_regions: std::sync::RwLock<std::collections::BTreeMap<usize, CowRegionInfo>>,
    /// If [`Self::initialize_boot_specific_kdf_support`] has been run, this is set to a value that
    /// is persistent across multiple process executions, however, it is ephemeral across true
    /// reboots.
    boot_id: std::sync::OnceLock<Vec<u8>>,
    stdio_is_tty: [bool; 3],
}

impl core::fmt::Debug for LinuxUserland {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LinuxUserland").finish_non_exhaustive()
    }
}

/// Information about a CoW-eligible memory region backed by a file.
#[derive(Debug, Clone)]
struct CowRegionInfo {
    /// The path to the backing file on the host filesystem.
    file_path: PathBuf,
    /// Length of the backing file.
    file_length: usize,
}

const IF_NAMESIZE: usize = 16;
/// Use TUN device
const IFF_TUN: i32 = 0x0001;
/// Do not provide packet information
const IFF_NO_PI: i32 = 0x1000;
/// libc `ifreq` structure, used for TUN/TAP devices.
#[repr(C)]
struct Ifreq {
    /// interface name, e.g. "en0"
    pub ifr_name: [i8; IF_NAMESIZE],
    pub ifr_ifru: Ifru,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Ifmap {
    mem_start: usize,
    mem_end: usize,
    base_addr: u16,
    irq: u8,
    dma: u8,
    port: u8,
}

/// libc `ifreq.ifr_ifru` union, used for TUN/TAP devices.
///
/// We only need `ifru_flags` for now; `ifru_map` is to ensure the size of the union
/// matches libc.
#[repr(C)]
pub union Ifru {
    // pub ifru_addr: crate::sockaddr,
    // pub ifru_dstaddr: crate::sockaddr,
    // pub ifru_broadaddr: crate::sockaddr,
    // pub ifru_netmask: crate::sockaddr,
    // pub ifru_hwaddr: crate::sockaddr,
    ifru_flags: i16,
    // pub ifru_ifindex: i32,
    // pub ifru_metric: i32,
    // pub ifru_mtu: i32,
    ifru_map: Ifmap,
    // pub ifru_slave: [i8; IF_NAMESIZE],
    // pub ifru_newname: [i8; IF_NAMESIZE],
    // pub ifru_data: *mut i8,
}

/// Opens `path` on the host, matching the real `open(2)` ABI (`(fd, errno)`-style raw syscall
/// return convention).
///
/// `open` does not exist as a syscall number on aarch64 (only `openat` does); this dispatches
/// to `openat(AT_FDCWD, path, flags, mode)` there and to `open(path, flags, mode)` on x86_64,
/// giving every call site a single, arch-neutral entry point instead of repeating the cfg-gate.
///
/// # Safety
/// `path` must be a valid, NUL-terminated pointer for the duration of the call.
unsafe fn raw_open(path: usize, flags: usize, mode: usize) -> Result<usize, syscalls::Errno> {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        syscalls::syscall3(syscalls::Sysno::open, path, flags, mode)
    }
    #[cfg(target_arch = "aarch64")]
    #[allow(clippy::cast_sign_loss, reason = "AT_FDCWD is a negative sentinel, passed as-is")]
    unsafe {
        syscalls::syscall4(
            syscalls::Sysno::openat,
            libc::AT_FDCWD as usize,
            path,
            flags,
            mode,
        )
    }
}

impl LinuxUserland {
    /// Create a new userland-Linux platform for use in `LiteBox`.
    ///
    /// Takes an optional tun device name (such as `"tun0"` or `"tun99"`) to connect networking (if
    /// not specified, networking is disabled).
    ///
    /// # Panics
    ///
    /// Panics if the tun device could not be successfully opened.
    pub fn new(tun_device_name: Option<&str>) -> &'static Self {
        register_exception_handlers();

        let tun_socket_fd = tun_device_name
            .map(|tun_device_name| {
                let tun_path = b"/dev/net/tun\0";
                let tun_fd = unsafe {
                    raw_open(
                        tun_path.as_ptr() as usize,
                        (litebox::fs::OFlags::RDWR
                            | litebox::fs::OFlags::CLOEXEC
                            | litebox::fs::OFlags::NONBLOCK)
                            .bits() as usize,
                        litebox::fs::Mode::empty().bits() as usize,
                    )
                }
                .expect("failed to open tun device");

                let tunsetiff = |fd: usize, ifreq: *const Ifreq| {
                    let cmd =
                        litebox_common_linux::iow!(b'T', 202, size_of::<::core::ffi::c_int>());
                    unsafe {
                        syscalls::syscall3(syscalls::Sysno::ioctl, fd, cmd as usize, ifreq as usize)
                    }
                    .expect("failed to set TUN interface flags");
                };
                let ifreq = Ifreq {
                    ifr_name: {
                        let mut name = [0i8; 16];
                        assert!(tun_device_name.len() < 16); // Note: strictly-less-than 16, to ensure it fits
                        for (i, b) in tun_device_name.char_indices() {
                            let b = b as u32;
                            assert!(b < 128);
                            name[i] = i8::try_from(b).unwrap();
                        }
                        name
                    },
                    ifr_ifru: Ifru {
                        // IFF_NO_PI: no tun header
                        // IFF_TUN: create tun (i.e., IP)
                        ifru_flags: i16::try_from(IFF_TUN | IFF_NO_PI).unwrap(),
                    },
                };
                tunsetiff(tun_fd, &raw const ifreq);

                // By taking ownership, we are letting the drop handler automatically run `libc::close`
                // when necessary.
                unsafe { std::os::fd::OwnedFd::from_raw_fd(tun_fd.reinterpret_as_signed().trunc()) }
            })
            .into();

        let reserved_pages = Self::read_maps();
        let platform = Self {
            tun_socket_fd,
            reserved_pages,
            cow_regions: std::sync::RwLock::new(std::collections::BTreeMap::new()),
            boot_id: std::sync::OnceLock::new(),
            stdio_is_tty: [
                std::io::stdin().is_terminal(),
                std::io::stdout().is_terminal(),
                std::io::stderr().is_terminal(),
            ],
        };
        Box::leak(Box::new(platform))
    }

    /// Initializes support for KDFs by using boot-specific uniqueness.
    ///
    /// NOTE: The boot-specific uniqueness is NOT secure against an adversary with code execution or
    /// file read permissions on the host file system, since other processes on the same system can
    /// also derive the exact same keys.
    ///
    /// # Panics
    ///
    /// Panics if some standard Linux kernel-provided files are not available/accessible.
    ///
    /// Panics if run more than once on the same platform instance.
    pub fn initialize_boot_specific_kdf_support(&self) {
        let parsed: Vec<u8> = std::fs::read("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim_ascii()
            .split(|&x| x == b'-')
            .flat_map(|chunk| {
                chunk
                    .chunks(2)
                    .map(|t| u8::from_str_radix(str::from_utf8(t).unwrap(), 16).unwrap())
            })
            .collect();
        assert_eq!(parsed.len(), 16);
        self.boot_id.set(parsed).unwrap();
    }

    /// Register a CoW-eligible memory region backed by a file.
    ///
    /// # Panics
    ///
    /// Panics if an overlapping region is already registered.
    pub fn register_cow_region(&self, data: &'static [u8], file_path: impl Into<PathBuf>) {
        let start = data.as_ptr() as usize;
        let info = CowRegionInfo {
            file_path: file_path.into(),
            file_length: data.len(),
        };

        let mut regions = self.cow_regions.write().unwrap();
        assert!(
            regions.range(start..start + data.len()).next().is_none(),
            "Attempting to register an overlapping region"
        );
        let old = regions.insert(start, info);
        assert!(old.is_none());
    }

    /// Look up the file backing a static slice for CoW mapping.
    ///
    /// Returns `Some((file_path, offset_in_file))` if the slice is backed by a registered
    /// CoW region, `None` otherwise.
    fn lookup_cow_region(&self, source_data: &'static [u8]) -> Option<(PathBuf, usize)> {
        let slice_start = source_data.as_ptr() as usize;
        let slice_len = source_data.len();

        let regions = self.cow_regions.read().unwrap();

        if let Some((&region_start, info)) = regions.range(..=slice_start).next_back() {
            let region_end = region_start.checked_add(info.file_length).unwrap();
            let slice_end = slice_start.checked_add(slice_len).unwrap();

            if slice_start >= region_start && slice_end <= region_end {
                return Some((info.file_path.clone(), slice_start - region_start));
            }
        }
        None
    }

    fn read_maps() -> alloc::vec::Vec<core::ops::Range<usize>> {
        // TODO: this function is not guaranteed to return all allocated pages, as it may
        // allocate more pages after the mapping file is read. Missing allocated pages may
        // cause the program to crash when calling `mmap` or `mremap` with the `MAP_FIXED` flag later.
        // We should either fix `mmap` to handle this error, or let global allocator call this function
        // whenever it get more pages from the host.
        let path = c"/proc/self/maps";
        let fd = unsafe { raw_open(path.as_ptr() as usize, OFlags::RDONLY.bits() as usize, 0) };
        let Ok(fd) = fd else {
            return alloc::vec::Vec::new();
        };
        let mut buf = [0u8; 8192];
        let mut total_read = 0;
        while total_read < buf.len() {
            let n = unsafe {
                syscalls::syscall3(
                    syscalls::Sysno::read,
                    fd,
                    buf.as_mut_ptr() as usize + total_read,
                    buf.len() - total_read,
                )
            }
            .expect("read failed");
            if n == 0 {
                break;
            }
            total_read += n;
        }
        assert!(total_read < buf.len(), "buffer too small");
        unsafe { syscalls::syscall1(syscalls::Sysno::close, fd) }.expect("close failed");

        let mut reserved_pages = alloc::vec::Vec::new();
        let s = core::str::from_utf8(&buf[..total_read]).expect("invalid UTF-8");
        for line in s.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 5 {
                continue;
            }
            let range = parts[0].split('-').collect::<Vec<&str>>();
            let start = usize::from_str_radix(range[0], 16).expect("invalid start address");
            let end = usize::from_str_radix(range[1], 16).expect("invalid end address");
            reserved_pages.push(start..end);
        }
        reserved_pages
    }

    #[expect(
        clippy::missing_panics_doc,
        reason = "panicking only on failures of documented linux contracts"
    )]
    pub fn init_task(&self) -> litebox_common_linux::TaskParams {
        let tid = unsafe { syscalls::raw::syscall0(syscalls::Sysno::gettid) }
            .try_into()
            .unwrap();
        let ppid = unsafe { syscalls::raw::syscall0(syscalls::Sysno::getppid) }
            .try_into()
            .unwrap();
        litebox_common_linux::TaskParams {
            pid: tid,
            ppid,
            uid: unsafe { syscalls::raw::syscall0(syscalls::Sysno::getuid) }
                .try_into()
                .unwrap(),
            euid: unsafe { syscalls::raw::syscall0(syscalls::Sysno::geteuid) }
                .try_into()
                .unwrap(),
            gid: unsafe { syscalls::raw::syscall0(syscalls::Sysno::getgid) }
                .try_into()
                .unwrap(),
            egid: unsafe { syscalls::raw::syscall0(syscalls::Sysno::getegid) }
                .try_into()
                .unwrap(),
        }
    }

    /// Wait until there is data available on the TUN device.
    ///
    /// # Panics
    ///
    /// Panics if the TUN device is not initialized.
    pub fn wait_on_tun(&self, timeout: Option<Duration>) {
        let tun_fd = self.tun_socket_fd.read().unwrap();
        let mut pfd = libc::pollfd {
            fd: tun_fd.as_ref().unwrap().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let _ = unsafe {
            libc::poll(
                &raw mut pfd,
                1,
                timeout.map_or(-1, |t| {
                    let ms = t.as_millis();
                    i32::try_from(ms).unwrap_or(i32::MAX)
                }),
            )
        };
    }

    /// Spawns the host-syscall-proxy thread (see [`aarch64_syscall_proxy`]'s module doc) and
    /// installs the seccomp filter. MUST be called in this order (the proxy thread must exist,
    /// unfiltered, before the filter goes on) and only once per process. On x86_64 the proxy
    /// thread is unnecessary (guest syscalls never reach this filter at all -- they're
    /// intercepted by the ELF-patched fast-path trampoline before ever executing `syscall`), so
    /// this is a thin wrapper calling the real `enable_seccomp_filter` directly there.
    #[cfg(target_arch = "aarch64")]
    pub fn enable_seccomp_filter() {
        aarch64_syscall_proxy::spawn();
        Self::enable_seccomp_filter_inner();
    }
    #[cfg(not(target_arch = "aarch64"))]
    pub fn enable_seccomp_filter() {
        Self::enable_seccomp_filter_inner();
    }

    #[allow(
        clippy::missing_panics_doc,
        reason = "the seccomp filter rules are hardcoded and not expected to fail"
    )]
    fn enable_seccomp_filter_inner() {
        use seccompiler::{
            BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition,
            SeccompFilter, SeccompRule,
        };

        let rules = vec![
            // TUN and terminal
            (libc::SYS_read, vec![]),
            (libc::SYS_write, vec![]),
            // `poll` does not exist as a syscall number on aarch64 (glibc's `poll()` calls
            // `ppoll` there instead).
            #[cfg(target_arch = "x86_64")]
            (libc::SYS_poll, vec![]),
            #[cfg(target_arch = "aarch64")]
            (libc::SYS_ppoll, vec![]),
            // memory management
            (libc::SYS_mmap, vec![]),
            (libc::SYS_mprotect, vec![]),
            (libc::SYS_munmap, vec![]),
            (libc::SYS_mremap, vec![]),
            // signal
            (libc::SYS_rt_sigreturn, vec![]),
            (libc::SYS_sigaltstack, vec![]),
            (libc::SYS_tgkill, vec![]),
            (libc::SYS_timer_create, vec![]),
            (libc::SYS_timer_settime, vec![]),
            (libc::SYS_timer_delete, vec![]),
            // called by [pthread_create](https://codebrowser.dev/glibc/glibc/nptl/pthread_create.c.html#83) to set up signal handler
            // to support setuid et.al. functions (which we probably don't need, but include them in debug mode to suppress the warnings
            // about missing seccomp rules for these syscalls).
            #[cfg(debug_assertions)]
            (libc::SYS_rt_sigaction, vec![]),
            // TODO: also called by `next_signal_handler`, but I'm not sure if it's really needed.
            (libc::SYS_rt_sigprocmask, vec![]),
            // thread management
            (libc::SYS_exit, vec![]),
            (libc::SYS_exit_group, vec![]),
            (libc::SYS_clone3, vec![]),
            // sync
            (libc::SYS_futex, vec![]),
            // misc
            (libc::SYS_getrandom, vec![]),
            // required by std spawn
            (libc::SYS_rseq, vec![]),
            (libc::SYS_set_robust_list, vec![]),
            (libc::SYS_get_robust_list, vec![]),
            (libc::SYS_sched_getaffinity, vec![]),
            (libc::SYS_gettid, vec![]),
            (libc::SYS_madvise, vec![]),
            // required by libc allocator
            (libc::SYS_brk, vec![]),
            (libc::SYS_getpid, vec![]),
            // TODO: could be removed if we pre-open files (see `try_allocate_cow_pages`)
            //
            // `open` does not exist as a syscall number on aarch64 (glibc always emits
            // `openat` there); only add this x86_64-specific legacy-syscall rule where the
            // syscall number actually exists.
            #[cfg(target_arch = "x86_64")]
            (
                libc::SYS_open,
                vec![
                    SeccompRule::new(vec![
                        SeccompCondition::new(
                            1,
                            SeccompCmpArgLen::Dword,
                            SeccompCmpOp::Eq,
                            u64::from(OFlags::RDONLY.bits()),
                        )
                        .unwrap(),
                    ])
                    .unwrap(),
                ],
            ),
            // aarch64 equivalent of the `open`(RDONLY) rule above: `raw_open` dispatches to
            // `openat(AT_FDCWD, path, flags, mode)` there, so the flags argument moves from
            // index 1 to index 2.
            #[cfg(target_arch = "aarch64")]
            (
                libc::SYS_openat,
                vec![
                    SeccompRule::new(vec![
                        SeccompCondition::new(
                            2,
                            SeccompCmpArgLen::Dword,
                            SeccompCmpOp::Eq,
                            u64::from(OFlags::RDONLY.bits()),
                        )
                        .unwrap(),
                    ])
                    .unwrap(),
                ],
            ),
            // close/dup/clock_gettime/set_tid_address/prlimit64/readlinkat/fstat are
            // deliberately NOT allow-listed here on aarch64 (they are on x86_64, see the
            // `not(aarch64)` arms below): unlike x86_64 (which intercepts every GUEST syscall
            // via the ELF-patched fast-path trampoline before it ever reaches the kernel),
            // aarch64 has no such trampoline yet, so an `Allow` rule here would let the GUEST's
            // own calls to these same syscall numbers bypass emulation entirely and hit the
            // real kernel directly -- live-confirmed: a guest's `close(3)` reaching the real
            // kernel and failing EBADF against the real host fd table instead of litebox's own
            // virtual one. These seven are still needed by litebox's OWN host-side runtime code
            // (fd cleanup, elapsed-time reads, glibc/std internals) -- handled instead by
            // `exception_signal_handler`'s aarch64 host-syscall-proxy branch (see
            // `aarch64_syscall_proxy`'s module doc), which discriminates host-vs-guest via the
            // same `in_guest` scratch flag the SIGSYS syscall-dispatch bridge uses, and forwards
            // only the host-code case to a dedicated, always-unfiltered proxy thread.
            #[cfg(not(target_arch = "aarch64"))]
            (libc::SYS_close, vec![]),
            #[cfg(not(target_arch = "aarch64"))]
            (libc::SYS_dup, vec![]),
            #[cfg(not(target_arch = "aarch64"))]
            (libc::SYS_clock_gettime, vec![]),
            #[cfg(not(target_arch = "aarch64"))]
            (libc::SYS_set_tid_address, vec![]),
            #[cfg(not(target_arch = "aarch64"))]
            (libc::SYS_prlimit64, vec![]),
            #[cfg(not(target_arch = "aarch64"))]
            (libc::SYS_readlinkat, vec![]),
            #[cfg(not(target_arch = "aarch64"))]
            (libc::SYS_fstat, vec![]),
        ];
        let rule_map: std::collections::BTreeMap<i64, Vec<SeccompRule>> =
            rules.into_iter().collect();
        let filter = SeccompFilter::new(
            rule_map,
            // In debug builds, log violations instead of silently returning an error so that
            // it won't fail silently during development (which may be hard to debug).
            if cfg!(debug_assertions) {
                SeccompAction::Trap
            } else {
                SeccompAction::Errno(libc::EINVAL.cast_unsigned())
            },
            SeccompAction::Allow,
            #[cfg(target_arch = "x86_64")]
            seccompiler::TargetArch::x86_64,
            #[cfg(target_arch = "aarch64")]
            seccompiler::TargetArch::aarch64,
        )
        .unwrap();
        // TODO: bpf program can be compiled offline
        let bpf_prog: BpfProgram = filter.try_into().unwrap();

        seccompiler::apply_filter(&bpf_prog).unwrap();
    }
}

/// A dedicated, always-unfiltered thread that performs real syscalls on behalf of HOST code
/// (litebox's own runtime/glibc/std internals) for the six syscall numbers that were removed
/// from the seccomp allow-list on aarch64.
///
/// Background: on aarch64 there is no ELF-patched fast-path trampoline (unlike x86_64), so
/// every GUEST syscall reaches the kernel directly and must be caught by the seccomp filter's
/// SIGSYS trap to be emulated (see `exception_signal_handler`'s aarch64 SIGSYS branch). But
/// litebox's own host-side code also needs a handful of these same syscall numbers (`close`,
/// `dup`, `clock_gettime`, `set_tid_address`, `prlimit64`, `readlinkat`, `fstat`) for entirely
/// unrelated reasons (fd cleanup, elapsed-time reads, glibc/std internals) -- and seccomp-bpf
/// has no way to see WHO issued a syscall (only the syscall number, arch, instruction pointer,
/// and up to 6 argument words), so a simple `Allow` rule for these would let a GUEST'S calls to
/// the same numbers bypass emulation entirely (live-confirmed: a guest's `close(3)` reaching the
/// real kernel directly, failing EBADF against the real process's fd table instead of going
/// through litebox's own virtual fd table).
///
/// This module closes that gap: instead of an `Allow` rule, these six syscalls use `Trap`
/// (SIGSYS) unconditionally, same as any other unhandled syscall. `exception_signal_handler`'s
/// aarch64 branch discriminates guest-vs-host the same way it already does for the
/// syscall-emulation dispatch (`in_guest` in the trapping thread's `Aarch64ThreadScratch`): a
/// guest-issued trap for these numbers still dispatches into `syscall_callback` for real
/// emulation (litebox's own `sys_close`/etc, which manage a virtual fd table), while a
/// host-issued trap for these numbers is forwarded to this proxy thread -- the ONLY thread in
/// the process that never has the seccomp filter installed (it's spawned before
/// `enable_seccomp_filter` ever runs, and a thread's seccomp filter is never retroactively
/// applied to threads that already exist) -- which performs the real syscall and returns the
/// result.
///
/// Communication is a single-slot mailbox (no heap allocation, no libc calls beyond `futex`,
/// which is itself already always allowed): a request is written under a spinlock (so
/// concurrent host threads serialize rather than racing), the proxy thread is woken via
/// `futex(FUTEX_WAKE)`, and the caller blocks on `futex(FUTEX_WAIT)` for the response -- both
/// safe to call from inside a signal handler (no allocation, no non-reentrant libc state).
#[cfg(target_arch = "aarch64")]
mod aarch64_syscall_proxy {
    use std::sync::atomic::{AtomicI64, AtomicU32, AtomicUsize, Ordering};

    const STATE_IDLE: u32 = 0;
    const STATE_REQUEST: u32 = 1;
    const STATE_RESPONSE: u32 = 2;

    struct Mailbox {
        /// Spinlock guarding request submission: only one host thread may have a request
        /// in flight at a time (0 = unlocked, 1 = locked).
        lock: AtomicU32,
        state: AtomicU32,
        sysno: AtomicI64,
        args: [AtomicUsize; 6],
        result: AtomicI64,
    }

    static MAILBOX: Mailbox = Mailbox {
        lock: AtomicU32::new(0),
        state: AtomicU32::new(STATE_IDLE),
        sysno: AtomicI64::new(0),
        args: [
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
        ],
        result: AtomicI64::new(0),
    };

    fn futex_wait(addr: &AtomicU32, expected: u32) {
        unsafe {
            let _ = syscalls::syscall4(
                syscalls::Sysno::futex,
                std::ptr::from_ref(addr) as usize,
                libc::FUTEX_WAIT as usize,
                expected as usize,
                0, // no timeout
            );
        }
    }

    fn futex_wake(addr: &AtomicU32) {
        unsafe {
            let _ = syscalls::syscall3(
                syscalls::Sysno::futex,
                std::ptr::from_ref(addr) as usize,
                libc::FUTEX_WAKE as usize,
                1,
            );
        }
    }

    /// Spawns the proxy thread. MUST be called before the seccomp filter is installed (see this
    /// module's doc comment) and only once.
    pub(super) fn spawn() {
        std::thread::Builder::new()
            .name("aarch64-syscall-proxy".to_owned())
            .spawn(run)
            .expect("failed to spawn aarch64 syscall proxy thread");
    }

    fn run() {
        loop {
            // Wait for a request. A spurious wake (state still IDLE) just loops back.
            loop {
                if MAILBOX.state.load(Ordering::Acquire) == STATE_REQUEST {
                    break;
                }
                futex_wait(&MAILBOX.state, STATE_IDLE);
            }
            let sysno = MAILBOX.sysno.load(Ordering::Relaxed);
            let args: [usize; 6] =
                std::array::from_fn(|i| MAILBOX.args[i].load(Ordering::Relaxed));
            let result = unsafe {
                syscalls::raw_syscall!(
                    syscalls::Sysno::new(sysno as usize).expect("invalid proxied syscall number"),
                    args[0],
                    args[1],
                    args[2],
                    args[3],
                    args[4],
                    args[5]
                )
            };
            MAILBOX.result.store(result as i64, Ordering::Relaxed);
            MAILBOX.state.store(STATE_RESPONSE, Ordering::Release);
            futex_wake(&MAILBOX.state);
        }
    }

    /// Proxies a syscall through the dedicated unfiltered thread and returns its real result.
    /// Called from `exception_signal_handler`'s SIGSYS handler for the host-code case -- must
    /// remain signal-safe (no allocation, no non-reentrant libc calls).
    pub(super) fn proxy(sysno: i64, args: [usize; 6]) -> i64 {
        // Acquire the single-slot mailbox's spinlock. Contention is expected to be rare (these
        // six syscalls are only host-runtime-init-path calls, not steady-state guest traffic),
        // so a spin (rather than a futex-based lock) keeps this path simple and allocation-free.
        while MAILBOX
            .lock
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::hint::spin_loop();
        }

        MAILBOX.sysno.store(sysno, Ordering::Relaxed);
        for (i, arg) in args.iter().enumerate() {
            MAILBOX.args[i].store(*arg, Ordering::Relaxed);
        }
        MAILBOX.state.store(STATE_REQUEST, Ordering::Release);
        futex_wake(&MAILBOX.state);

        loop {
            if MAILBOX.state.load(Ordering::Acquire) == STATE_RESPONSE {
                break;
            }
            futex_wait(&MAILBOX.state, STATE_REQUEST);
        }
        let result = MAILBOX.result.load(Ordering::Relaxed);
        MAILBOX.state.store(STATE_IDLE, Ordering::Release);
        MAILBOX.lock.store(0, Ordering::Release);
        result
    }
}

impl litebox::platform::Provider for LinuxUserland {}

impl litebox::platform::SignalProvider for LinuxUserland {
    type Signal = litebox_common_linux::signal::Signal;

    fn take_pending_signals(&self, mut f: impl FnMut(Self::Signal)) {
        let sigs = take_pending_host_signals();
        for sig in sigs {
            f(sig);
        }
    }
}

/// Atomically takes the per-thread pending host signal bitmask.
#[cfg(target_arch = "x86_64")]
fn take_pending_host_signals() -> litebox_common_linux::signal::SigSet {
    // Atomically swap the per-thread pending signals with zero.
    // Only the low 32 bits are used (covers traditional signals 1-31).
    let lo: u32;
    unsafe {
        core::arch::asm!(
            "xor {tmp:e}, {tmp:e}",
            concat!("xchg DWORD PTR ", tls!("pending_host_signals"), ", {tmp:e}"),
            tmp = out(reg) lo,
            options(nostack)
        );
    }
    litebox_common_linux::signal::SigSet::from_u64(u64::from(lo))
}

/// AArch64 variant. `pending_host_signals` is only ever written by
/// [`record_pending_signal`], which runs strictly on THIS thread's own execution (a signal
/// handler fully interrupts and returns to the same thread it interrupted -- per POSIX, no
/// other thread can concurrently write it), so a plain (non-atomic-instruction) swap is
/// sufficient here, mirroring x86_64's `xchg` in effect if not in literal instruction form.
#[cfg(target_arch = "aarch64")]
fn take_pending_host_signals() -> litebox_common_linux::signal::SigSet {
    let ptr = aarch64_scratch_or_host_only();
    let lo = unsafe {
        let val = core::ptr::read_volatile(&raw const (*ptr).pending_host_signals);
        core::ptr::write_volatile(&raw mut (*ptr).pending_host_signals, 0);
        val
    };
    litebox_common_linux::signal::SigSet::from_u64(u64::from(lo))
}

/// Runs a guest thread using the provided shim and the given initial context.
///
/// This will run until the thread terminates or returns.
///
/// # Safety
/// The context must be valid guest context.
pub unsafe fn run_thread<T>(shim: T, ctx: &mut litebox_common_linux::PtRegs)
where
    T: litebox::shim::EnterShim<ExecutionContext = litebox_common_linux::PtRegs>,
{
    run_thread_inner(&shim, ctx, false);
}

/// Run a guest thread using a reference to the shim.
///
/// Unlike `run_thread`, this version takes a reference instead of ownership,
/// avoiding struct moves that could invalidate internal state.
///
/// # Safety
/// The context must be valid guest context.
pub unsafe fn run_thread_ref<T>(shim: &T, ctx: &mut litebox_common_linux::PtRegs)
where
    T: litebox::shim::EnterShim<ExecutionContext = litebox_common_linux::PtRegs>,
{
    run_thread_inner(shim, ctx, false);
}

/// Re-enter a guest thread using a reference to the shim.
///
/// This version takes a reference instead of ownership, avoiding struct moves
/// that could invalidate internal state.
///
/// # Safety
/// The context must be valid guest context.
pub unsafe fn reenter_thread<T>(shim: &T, ctx: &mut litebox_common_linux::PtRegs)
where
    T: litebox::shim::EnterShim<ExecutionContext = litebox_common_linux::PtRegs>,
{
    run_thread_inner(shim, ctx, true);
}

struct ThreadContext<'a> {
    shim: &'a dyn litebox::shim::EnterShim<ExecutionContext = litebox_common_linux::PtRegs>,
    ctx: &'a mut litebox_common_linux::PtRegs,
}

fn run_thread_inner(
    shim: &dyn litebox::shim::EnterShim<ExecutionContext = litebox_common_linux::PtRegs>,
    ctx: &mut litebox_common_linux::PtRegs,
    reenter: bool,
) {
    let ctx_ptr = core::ptr::from_mut(ctx);
    let mut thread_ctx = ThreadContext { shim, ctx };
    ThreadHandle::run_with_handle(|| {
        #[cfg(target_arch = "x86_64")]
        with_signal_alt_stack(|_alt_stack_base| unsafe {
            run_thread_arch(&mut thread_ctx, ctx_ptr, u8::from(reenter));
        });
        #[cfg(target_arch = "aarch64")]
        with_signal_alt_stack(|alt_stack_base| unsafe {
            run_thread_arch(
                &mut thread_ctx,
                ctx_ptr,
                u8::from(reenter),
                aarch64_scratch_from_alt_stack_base(alt_stack_base),
            );
        });
    });
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    "
    .section .tbss
    .align 8
scratch:
    .quad 0
host_sp:
    .quad 0
host_bp:
    .quad 0
guest_context_top:
    .quad 0
.globl guest_fsbase
guest_fsbase:
    .quad 0
in_guest:
    .byte 0
.globl interrupt
interrupt:
    .byte 0
    .align 4
.globl pending_host_signals
pending_host_signals:
    .long 0
    .align 8
.globl wait_waker_addr
wait_waker_addr:
    .quad 0
    "
);

#[cfg(target_arch = "x86_64")]
fn set_guest_fsbase(value: usize) {
    unsafe {
        core::arch::asm! {
            "mov fs:guest_fsbase@tpoff, {}",
            in(reg) value,
            options(nostack, preserves_flags)
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn get_guest_fsbase() -> usize {
    let value: usize;
    unsafe {
        core::arch::asm! {
            "mov {}, fs:guest_fsbase@tpoff",
            out(reg) value,
            options(nostack, preserves_flags)
        }
    }
    value
}

/// Per-thread host/guest transition scratch state, aarch64 only.
///
/// AArch64 has exactly one EL0-writable TLS-base register (`TPIDR_EL0`) -- unlike x86_64's
/// independent FS/GS pair, there is no second register the host can use to stash its own
/// per-thread state while the guest owns the TLS register for its own use. Real sandboxes
/// (gVisor, QEMU) handle this by explicitly serializing the swap and finding host scratch
/// state via a lookup that does NOT depend on TPIDR_EL0 already being correct.
///
/// This struct is placed directly below the guard page of each guest thread's alternate
/// signal stack (see [`with_signal_alt_stack`]/[`AARCH64_SCRATCH_SIZE`]), which lets a signal
/// handler recover its address purely from `context.uc_stack.ss_sp` (populated by the kernel
/// from whichever `sigaltstack` was active when the signal was delivered) -- no TLS access,
/// no syscall, no global table, before this struct's own `host_tpidr` field can be restored
/// into the hardware register.
#[cfg(target_arch = "aarch64")]
#[repr(C)]
struct Aarch64ThreadScratch {
    /// A fixed sentinel value (never anything else), written once when this struct is placed
    /// below a guest thread's alternate signal stack. `context.uc_stack.ss_sp` reports the
    /// PREVIOUSLY REGISTERED alt-stack config even for a thread that never registered one at
    /// all (verified live: the kernel does NOT null it out or set `SS_DISABLE` -- it reports
    /// arbitrary primary-stack-derived values), so `ss_sp`'s mere non-nullness is not
    /// sufficient evidence this is really a guest thread's alt-stack. This magic number, at a
    /// fixed offset from the candidate `mapping_base`, is what actually verifies it -- read
    /// this field FIRST and bail out on any mismatch before trusting anything else here.
    magic: u64,
    /// The host's own `TPIDR_EL0` value (this thread's real pthread/glibc TLS base),
    /// saved here immediately before switching to the guest and restored from here as the
    /// very first thing any signal handler (or `syscall_callback`) does.
    host_tpidr: usize,
    /// Host stack pointer, saved across the switch to guest context.
    host_sp: usize,
    /// Host frame pointer (`x29`), saved across the switch to guest context.
    host_fp: usize,
    /// Address one-past-the-end of the guest [`litebox_common_linux::PtRegs`] on the host
    /// stack -- the top of the region `syscall_callback` pushes the trapped guest register
    /// state into.
    guest_context_top: usize,
    /// The guest's own `TPIDR_EL0` value, restored into the hardware register on every
    /// guest re-entry (mirrors x86_64's `guest_fsbase`).
    guest_tpidr: usize,
    /// Whether the thread is currently executing guest code (`1`) or host code (`0`).
    /// Read/written as a single byte so a signal handler can safely inspect and clear it
    /// without a wider (potentially non-atomic-with-respect-to-the-interrupted-code) access.
    in_guest: u8,
    /// Set by [`interrupt_signal_handler`] when it needs `run_thread_arch` to route through
    /// `interrupt_callback` instead of resuming the guest normally.
    interrupt: u8,
    _pad: [u8; 2],
    /// Bitmask of traditional signals (1..=31) pending delivery to the shim, set by
    /// [`record_pending_signal`] and atomically drained by [`take_pending_host_signals`].
    pending_host_signals: u32,
    /// Address of a `Box<Waker>` to notify when a host signal becomes pending, or 0 if none
    /// is currently registered (mirrors x86_64's `wait_waker_addr`).
    wait_waker_addr: usize,
    /// The real resume PC for a guest syscall reached via the SIGSYS fallback path (an
    /// unpatched `svc #0`, see `exception_signal_handler`'s aarch64 SIGSYS branch), or 0 when
    /// not applicable (the normal fast-path `bl`-trampoline entry, where x30/LR IS already the
    /// correct resume address and this field is unused).
    ///
    /// `syscall_callback`'s register-dump asm writes BOTH `PtRegs.regs[30]` (the guest's own
    /// return-address-for-its-caller) and `PtRegs.pc` (where to resume the guest) from the SAME
    /// x30 register -- correct only for the fast path, where a `bl` naturally sets LR to the
    /// resume point (the guest's own real return address is separately preserved by normal
    /// AAPCS64 call-site convention, spilled to the guest's stack by its caller before the
    /// call). A trapped `svc #0` does NOT touch LR at all: x30 at signal-delivery time is
    /// whatever the guest's OWN in-flight call chain has it set to (its real, needed
    /// return-address for whatever function contains the `svc`), and overwriting it with the
    /// resume PC -- as an earlier version of this fix did -- corrupts that value, so when the
    /// syscall wrapper function later executes `ret`, it jumps to the (former) resume PC
    /// instead of its real caller, immediately re-executing that same address forever (a
    /// deterministic infinite loop, confirmed live: `PtRegs.regs[30]` and `PtRegs.pc` observed
    /// byte-identical after a SIGSYS-dispatched syscall, both holding the resume address,
    /// neither holding the guest's real call-return address). This field lets
    /// `exception_signal_handler` stash the correct resume PC separately, and
    /// `syscall_callback`'s dump patches `PtRegs.pc` from here instead of from x30 whenever
    /// it's nonzero, leaving `PtRegs.regs[30]`/the guest's real x30 untouched.
    svc_resume_pc: usize,
    /// The guest's real x3 (its 4th syscall argument) for a SIGSYS-dispatched syscall, or 0
    /// when not applicable. `set_signal_return` unconditionally overwrites `regs[3]` with the
    /// scratch pointer (every callback's very first instruction reads `[x3, #16]`, so this
    /// can't be avoided) -- destroying the guest's real x3 before `syscall_callback`'s dump ever
    /// runs, corrupting the 4th argument of any syscall reached this way (live-confirmed via
    /// `openat(path, flags, mode)`: `PtRegs.regs[3]`/`mode` read back as the scratch pointer's
    /// address reinterpreted as a mode bitmask). Mirrors `svc_resume_pc`'s pattern: stashed here
    /// before dispatch, patched into `PtRegs.regs[3]` by `syscall_callback`'s dump when nonzero.
    svc_real_x3: usize,
}

#[cfg(target_arch = "aarch64")]
thread_local! {
    /// Points at the current thread's [`Aarch64ThreadScratch`]. For a genuine guest thread,
    /// [`run_thread_arch`] overrides this to point at the alt-stack-backed struct that its own
    /// signal-handler paths can also recover via `context.uc_stack.ss_sp` (see that type's doc
    /// comment). For any OTHER thread (a plain host thread calling `update_waker`/
    /// `take_pending_host_signals` -- e.g. a thread merely waiting on an epoll/futex, never a
    /// guest at all), this lazily allocates a private, heap-backed scratch struct on first use:
    /// unlike x86_64 (where every thread transparently has a `.tbss` slot for this regardless of
    /// whether it is a guest thread), aarch64 has no such free per-thread storage, so a plain
    /// host thread needs its own real backing struct to match x86_64's behavior of these
    /// functions succeeding on any thread, not just guest ones. Reading/writing this
    /// thread_local itself is only ever done from normal host code (never from inside a signal
    /// handler while a guest owns `TPIDR_EL0`), so it can safely use ordinary Rust TLS.
    static AARCH64_SCRATCH_PTR: core::cell::Cell<*mut Aarch64ThreadScratch> =
        const { core::cell::Cell::new(core::ptr::null_mut()) };
    static AARCH64_HOST_ONLY_SCRATCH: core::cell::RefCell<Option<alloc::boxed::Box<Aarch64ThreadScratch>>> =
        const { core::cell::RefCell::new(None) };
}

/// Returns this thread's [`Aarch64ThreadScratch`] pointer, lazily allocating a private
/// host-only one if this thread never went through [`run_thread_arch`] -- see
/// `AARCH64_SCRATCH_PTR`'s doc comment.
#[cfg(target_arch = "aarch64")]
fn aarch64_scratch_or_host_only() -> *mut Aarch64ThreadScratch {
    let ptr = AARCH64_SCRATCH_PTR.get();
    if !ptr.is_null() {
        return ptr;
    }
    AARCH64_HOST_ONLY_SCRATCH.with_borrow_mut(|slot| {
        let scratch = slot.get_or_insert_with(|| {
            alloc::boxed::Box::new(Aarch64ThreadScratch {
                magic: AARCH64_SCRATCH_MAGIC,
                host_tpidr: 0,
                host_sp: 0,
                host_fp: 0,
                guest_context_top: 0,
                guest_tpidr: 0,
                in_guest: 0,
                interrupt: 0,
                _pad: [0; 2],
                pending_host_signals: 0,
                wait_waker_addr: 0,
                svc_resume_pc: 0,
                svc_real_x3: 0,
            })
        });
        core::ptr::from_mut(scratch.as_mut())
    })
}

#[cfg(target_arch = "aarch64")]
fn aarch64_scratch_from_alt_stack_base(alt_stack_base: *mut u8) -> *mut Aarch64ThreadScratch {
    alt_stack_base.cast::<Aarch64ThreadScratch>()
}

#[cfg(target_arch = "aarch64")]
fn set_guest_tpidr(value: usize) {
    let ptr = AARCH64_SCRATCH_PTR.get();
    debug_assert!(!ptr.is_null(), "guest_tpidr set outside a guest thread");
    unsafe { (*ptr).guest_tpidr = value };
}

#[cfg(target_arch = "aarch64")]
fn get_guest_tpidr() -> usize {
    let ptr = AARCH64_SCRATCH_PTR.get();
    debug_assert!(!ptr.is_null(), "guest_tpidr read outside a guest thread");
    unsafe { (*ptr).guest_tpidr }
}

#[cfg(target_arch = "aarch64")]
impl litebox::platform::ArchSpecificProvider for LinuxUserland {
    fn set_arch_specific_register(
        &self,
        reg: &litebox::platform::ArchSpecificRegister,
        val: usize,
    ) -> Result<(), litebox::platform::ArchSpecificError> {
        match reg {
            litebox::platform::ArchSpecificRegister::TpidrEl0 => {
                if litebox_common_linux::arch::is_valid_user_fs_base(val) {
                    set_guest_tpidr(val);
                    Ok(())
                } else {
                    Err(litebox::platform::ArchSpecificError::RegisterUnpermittedValue)
                }
            }
            _ => Err(litebox::platform::ArchSpecificError::RegisterUnsupported),
        }
    }
    fn get_arch_specific_register(
        &self,
        reg: &litebox::platform::ArchSpecificRegister,
    ) -> Result<usize, litebox::platform::ArchSpecificError> {
        match reg {
            litebox::platform::ArchSpecificRegister::TpidrEl0 => Ok(get_guest_tpidr()),
            _ => Err(litebox::platform::ArchSpecificError::RegisterUnsupported),
        }
    }
}

/// Runs the guest thread until it terminates.
///
/// This saves all non-volatile register state then switches to the guest
/// context. When the guest makes a syscall, it jumps back into the middle of
/// this routine, at `syscall_callback`. This code then updates the guest
/// context structure, switches back to the host stack, and calls the syscall
/// handler.
///
/// When the guest thread terminates, this function returns after restoring
/// non-volatile register state.
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
unsafe extern "C-unwind" fn run_thread_arch(
    thread_ctx: &mut ThreadContext,
    ctx: *mut litebox_common_linux::PtRegs,
    reenter: u8,
) {
    core::arch::naked_asm!(
    "
    .cfi_startproc
    // Push all non-volatiles.
    push rbp
    mov rbp, rsp
    .cfi_def_cfa rbp, 16
    push rbx
    push r12
    push r13
    push r14
    push r15
    push rdi // save thread context

    // Save host rsp and rbp and guest context top in TLS.
    mov fs:host_sp@tpoff, rsp
    mov fs:host_bp@tpoff, rbp
    lea r8, [rsi + {GUEST_CONTEXT_SIZE}]
    mov fs:guest_context_top@tpoff, r8

    // Save host fs base in gs base. This will stay set for the lifetime
    // of this call stack.
    rdfsbase r8
    wrgsbase r8

    // Call init_handler or reenter_handler based on reenter flag (in dl).
    test dl, dl
    jnz 1f
    call {init_handler}
    jmp .Ldone
1:
    call {reenter_handler}
    jmp .Ldone

    // This entry point is called from the guest when it issues a syscall
    // instruction.
    //
    // At entry, the register context is the guest context with the
    // return address in rcx. r11 is an available scratch register (it would
    // contain rflags if the syscall instruction had actually been issued).
    .globl syscall_callback
syscall_callback:
    // Clear in_guest flag. This must be the first instruction to match the
    // expectations of `interrupt_signal_handler`.
    mov      BYTE PTR gs:in_guest@tpoff, 0

    // Restore host fs base.
    rdfsbase r11
    mov      gs:guest_fsbase@tpoff, r11
    rdgsbase r11
    wrfsbase r11

    // Switch to the top of the guest context.
    mov     r11, rsp
    mov     rsp, fs:guest_context_top@tpoff

    // TODO: save float and vector registers (xsave or fxsave)
    // Save caller-saved registers
    push    0x2b       // pt_regs->ss = __USER_DS
    push    r11        // pt_regs->sp
    pushfq             // pt_regs->eflags
    push    0x33       // pt_regs->cs = __USER_CS
    push    rcx        // pt_regs->ip
    push    rax        // pt_regs->orig_ax

    push    rdi         // pt_regs->di
    push    rsi         // pt_regs->si
    push    rdx         // pt_regs->dx
    push    rcx         // pt_regs->cx
    push    -38         // pt_regs->ax = ENOSYS
    push    r8          // pt_regs->r8
    push    r9          // pt_regs->r9
    push    r10         // pt_regs->r10
    push    [rsp + 88]  // pt_regs->r11 = rflags
    push    rbx         // pt_regs->bx
    push    rbp         // pt_regs->bp
    push    r12         // pt_regs->r12
    push    r13         // pt_regs->r13
    push    r14         // pt_regs->r14
    push    r15         // pt_regs->r15

    // Restore the stack and frame pointer.
    mov     rsp, fs:host_sp@tpoff
    mov     rbp, fs:host_bp@tpoff

    // Handle the syscall. This will jump back to the guest but
    // will return if the thread is exiting.
    mov rdi, [rsp] // pass thread_ctx
    call {syscall_handler}
    // This thread is done. Return.
    jmp .Ldone

exception_callback:
    // Restore the stack and frame pointer.
    mov     rsp, fs:host_sp@tpoff
    mov     rbp, fs:host_bp@tpoff

    mov rdi, [rsp] // pass thread_ctx
    call {exception_handler}
    jmp .Ldone

interrupt_callback:
    // Restore the stack and frame pointer.
    mov     rsp, fs:host_sp@tpoff
    mov     rbp, fs:host_bp@tpoff

    mov rdi, [rsp] // pass thread_ctx
    call {interrupt_handler}

.Ldone:

    lea  rsp, [rbp - 5*8]
    pop  r15
    pop  r14
    pop  r13
    pop  r12
    pop  rbx
    pop  rbp
    .cfi_def_cfa rsp, 8
    ret
    .cfi_endproc
",
    GUEST_CONTEXT_SIZE = const core::mem::size_of::<litebox_common_linux::PtRegs>(),
    init_handler = sym init_handler,
    reenter_handler = sym reenter_handler,
    syscall_handler = sym syscall_handler,
    exception_handler = sym exception_handler,
    interrupt_handler = sym interrupt_handler,
    );
}

/// AArch64 equivalent of `run_thread_arch` above -- same overall shape (save host
/// non-volatiles, remember where the guest context lives, dispatch to `init_handler` or
/// `reenter_handler`, provide `syscall_callback`/`exception_callback`/`interrupt_callback`
/// re-entry points), adapted for the AAPCS64 calling convention and for AArch64 having no
/// hardware `syscall`/`sysret`-equivalent fast path: `syscall_callback` is reached only via a
/// `bl` instruction that `litebox_syscall_rewriter` patches directly into the guest binary in
/// place of `svc #0` (or, for any unpatched/dynamically-generated `svc #0`, via the SIGSYS
/// signal handler jumping here through [`set_signal_return`]).
///
/// `x0` = `thread_ctx`, `x1` = `ctx` (guest [`litebox_common_linux::PtRegs`]*), `x2` = `reenter`
/// (0 or 1), `x3` = this thread's [`Aarch64ThreadScratch`]* (see that type's doc comment).
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
unsafe extern "C-unwind" fn run_thread_arch(
    thread_ctx: &mut ThreadContext,
    ctx: *mut litebox_common_linux::PtRegs,
    reenter: u8,
    scratch: *mut Aarch64ThreadScratch,
) {
    core::arch::naked_asm!(
    "
    .cfi_startproc
    // Push all non-volatiles (x19-x28), the frame-record pair (x29, x30), plus x0 (thread_ctx)
    // and x3 (scratch) which are needed again at .Ldone/the *_callback labels.
    stp x29, x30, [sp, #-16]!
    mov x29, sp
    .cfi_def_cfa w29, 16
    .cfi_offset w30, -8
    .cfi_offset w29, -16
    stp x19, x20, [sp, #-16]!
    stp x21, x22, [sp, #-16]!
    stp x23, x24, [sp, #-16]!
    stp x25, x26, [sp, #-16]!
    stp x27, x28, [sp, #-16]!
    stp x0, x3, [sp, #-16]!   // save thread_ctx (x0) and scratch (x3)

    // Save host sp/fp and guest context top into the scratch struct.
    mov x9, sp
    str x9, [x3, #16]          // scratch->host_sp (offset 16, see Aarch64ThreadScratch layout)
    str x29, [x3, #24]         // scratch->host_fp (offset 24)
    add x9, x1, {GUEST_CONTEXT_SIZE}
    str x9, [x3, #32]          // scratch->guest_context_top (offset 32)

    // Save the host's current TPIDR_EL0 into the scratch struct -- this is what every signal
    // handler restores as its very first action (see Aarch64ThreadScratch's doc comment).
    mrs x9, tpidr_el0
    str x9, [x3, #8]           // scratch->host_tpidr (offset 8)

    // Record this thread's scratch pointer in Rust TLS -- safe here since TPIDR_EL0 still
    // holds the host's own value at this point (we haven't switched to the guest yet).
    //
    // `scratch_entry_setup` is an ordinary Rust function call: per AAPCS64 it is free to
    // clobber x1-x18, x30 (every caller-saved register/LR except x0's own argument use), so x2
    // (the reenter flag, still needed right after this call) MUST be preserved across it
    // explicitly on the stack -- not in another register, since any other caller-saved
    // register is equally at risk of being clobbered by the call.
    str x2, [sp, #-16]!
    mov x0, x3
    bl {scratch_entry_setup}
    ldp x0, x3, [x29, #-{THREAD_CTX_SAVE_OFFSET}]
    ldr x2, [sp], #16

    // Dispatch to init_handler or reenter_handler based on the reenter flag (x2).
    cbnz w2, 1f
    bl {init_handler}
    b .Ldone
1:
    bl {reenter_handler}
    b .Ldone

    // Reached via a `bl` that litebox_syscall_rewriter patches in place of `svc #0` in the
    // guest binary (fast path, no kernel round-trip), or via the SIGSYS signal handler for an
    // unpatched trap (slow path) -- either way, x30 (LR) holds the guest's own return address
    // and the guest's other registers are still live exactly as the guest left them.
    .globl syscall_callback
syscall_callback:
    // Clear in_guest. Must be the very first store, matching interrupt_signal_handler's
    // expectations (it treats a clear in_guest as \"safe, not mid-guest-execution\").
    mov x9, xzr
    strb w9, [x3, #48]          // scratch->in_guest = 0 (offset 48)

    // Save the guest's TPIDR_EL0 (its own TLS base) into the scratch struct, then restore the
    // host's TPIDR_EL0 so host code (including the Rust handlers called below) has correct TLS.
    mrs x9, tpidr_el0
    str x9, [x3, #40]           // scratch->guest_tpidr (offset 40)
    ldr x9, [x3, #8]            // scratch->host_tpidr
    msr tpidr_el0, x9

    // Switch to the top of the guest context region and dump the full guest register file
    // there, matching PtRegs<aarch64>'s layout: regs[31], sp, pc, pstate, orig_x0, syscallno,
    // unused2.
    mov x10, sp                 // remember the guest sp before switching
    ldr x11, [x3, #32]          // scratch->guest_context_top
    mov sp, x11

    // regs[0..31): x0-x30, in order (x30/LR holds the guest's return address, i.e. PtRegs.pc
    // on syscall entry).
    //
    // The pre-index decrement here MUST equal `size_of::<PtRegs>()` (`GUEST_CONTEXT_SIZE`),
    // not merely `size_of::<[usize; 31]>()` (256) -- `guest_context_top` (loaded into x11
    // above) is one-past-the-end of the WHOLE `PtRegs` struct (regs[] + sp + pc + pstate +
    // orig_x0 + syscallno + unused2 = 288 bytes, not just the 256-byte regs[] array), matching
    // its definition at `run_thread_arch`'s prologue (`add x9, x1, {GUEST_CONTEXT_SIZE}`). A
    // literal `#-256` here under-decrements by 32 bytes, landing `sp` 32 bytes INSIDE the
    // struct instead of at its start -- every subsequent `[sp, #N]` write in this dump then
    // lands 32 bytes past its intended `PtRegs` field (`syscallno` ends up overlapping past the
    // struct's real end into whatever memory follows it), corrupting `PtRegs.syscallno` (read
    // back by the shim as garbage) and everything else in this dump -- confirmed live: the
    // guest's real x8=25 (`fcntl`) at trap time, but `PtRegs.syscallno` read back as
    // 0xffefec90, byte-identical to a stale guest stack-pointer value sitting exactly 32 bytes
    // beyond where `syscallno` should have landed.
    stp x0, x1, [sp, #-{GUEST_CONTEXT_SIZE}]!
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    stp x20, x21, [sp, #160]
    stp x22, x23, [sp, #176]
    stp x24, x25, [sp, #192]
    stp x26, x27, [sp, #208]
    stp x28, x29, [sp, #224]
    str x30, [sp, #240]         // regs[30]
    str x10, [sp, #248]         // PtRegs.sp -- the guest's own sp, saved above in x10
    str x30, [sp, #256]         // PtRegs.pc -- syscall return address (from x30/LR)

    // If this dispatch came via the SIGSYS fallback path (an unpatched `svc #0`, see
    // `exception_signal_handler`'s aarch64 branch), two live registers here are wrong and need
    // patching from values `exception_signal_handler` stashed in the scratch struct before the
    // jump: x30/LR is the GUEST's real return-address for its own caller, not the resume PC --
    // already correctly dumped as `regs[30]` above, but WRONG as `PtRegs.pc` (just written from
    // that same x30) -- and x3 is the scratch pointer this whole trampoline depends on, not the
    // guest's real x3 (its 4th syscall argument), already (wrongly) dumped as `regs[3]` by `stp
    // x2, x3, [sp, #16]` above. `scratch->svc_resume_pc` is the gate for BOTH patches (nonzero
    // exactly when this dispatch came via SIGSYS -- a real resume PC of address 0 is
    // impossible, so this is an unambiguous sentinel; `svc_real_x3` can legitimately BE zero
    // as a real argument value, so it can't gate itself the same way, but it's only ever
    // written together with `svc_resume_pc` and so shares its gate safely).
    ldr x9, [x3, #64]           // scratch->svc_resume_pc
    cbz x9, .Lno_svc_dispatch_fixup
    str x9, [sp, #256]          // overwrite PtRegs.pc with the real resume address
    str xzr, [x3, #64]          // clear scratch->svc_resume_pc
    ldr x9, [x3, #72]           // scratch->svc_real_x3
    str x9, [sp, #24]           // overwrite PtRegs.regs[3] with the guest's real x3
    str xzr, [x3, #72]          // clear scratch->svc_real_x3
.Lno_svc_dispatch_fixup:
    mrs x9, nzcv
    str x9, [sp, #264]          // PtRegs.pstate (condition flags only; enough for signal
                                 // delivery/restore round-tripping through this shim)
    ldr x9, [sp]                 // PtRegs.regs[0] (the original x0)
    str x9, [sp, #272]           // PtRegs.orig_x0
    mov w9, #0                   // matches x86_64's `push -38` (ENOSYS) landing in orig_ax's
                                  // sibling slot there; on aarch64 there is no direct
                                  // equivalent slot to pre-poison, so this is a no-op placeholder
                                  // kept only so the two trampolines' structure stays easy to
                                  // compare -- syscallno is set for real immediately below.
    ldr w9, [sp, #64]            // x8 (syscall number) was stored at regs[8], offset 64
    str w9, [sp, #280]           // PtRegs.syscallno

    // Restore the host stack and frame pointer.
    ldr x9, [x3, #16]           // scratch->host_sp
    mov sp, x9
    ldr x29, [x3, #24]          // scratch->host_fp

    // Handle the syscall. Returns only if the thread is exiting.
    ldp x0, x3, [x29, #-{THREAD_CTX_SAVE_OFFSET}]  // reload thread_ctx (x0), scratch (x3)
    bl {syscall_handler}
    b .Ldone

exception_callback:
    ldr x9, [x3, #16]           // scratch->host_sp
    mov sp, x9
    ldr x29, [x3, #24]          // scratch->host_fp
    ldp x0, x3, [x29, #-{THREAD_CTX_SAVE_OFFSET}]
    bl {exception_handler}
    b .Ldone

interrupt_callback:
    ldr x9, [x3, #16]           // scratch->host_sp
    mov sp, x9
    ldr x29, [x3, #24]          // scratch->host_fp
    ldp x0, x3, [x29, #-{THREAD_CTX_SAVE_OFFSET}]
    bl {interrupt_handler}

.Ldone:
    sub sp, x29, #{NONVOLATILE_SAVE_SIZE}
    ldp x0, x3, [sp], #16
    ldp x27, x28, [sp], #16
    ldp x25, x26, [sp], #16
    ldp x23, x24, [sp], #16
    ldp x21, x22, [sp], #16
    ldp x19, x20, [sp], #16
    ldp x29, x30, [sp], #16
    .cfi_def_cfa wsp, 0
    ret
    .cfi_endproc
",
    GUEST_CONTEXT_SIZE = const core::mem::size_of::<litebox_common_linux::PtRegs>(),
    // Offset from the saved x29 (frame pointer, pointing at the {x29,x30} pair pushed in the
    // prologue) back down to the {x0,x3} pair pushed right after it -- 6 further 16-byte pairs
    // (x19/20, x21/22, x23/24, x25/26, x27/28, x0/x3) below the frame-record pair itself.
    THREAD_CTX_SAVE_OFFSET = const 6 * 16,
    NONVOLATILE_SAVE_SIZE = const 6 * 16,
    scratch_entry_setup = sym aarch64_scratch_entry_setup,
    init_handler = sym init_handler,
    reenter_handler = sym reenter_handler,
    syscall_handler = sym syscall_handler,
    exception_handler = sym exception_handler,
    interrupt_handler = sym interrupt_handler,
    );
}

/// Records `scratch` as the current thread's [`Aarch64ThreadScratch`] pointer in Rust TLS.
/// Called once, early, from [`run_thread_arch`]'s prologue (aarch64 only), before the guest
/// is ever entered -- at that point `TPIDR_EL0` still holds the host's own value, so this
/// ordinary `thread_local!` write is safe.
#[cfg(target_arch = "aarch64")]
extern "C" fn aarch64_scratch_entry_setup(scratch: *mut Aarch64ThreadScratch) {
    AARCH64_SCRATCH_PTR.set(scratch);
}

/// Switches to the provided guest context.
///
/// # Safety
/// The context must be valid guest context. This can only be called if
/// `run_thread_arch` is on the stack; after the guest exits, it will return to
/// the interior of `run_thread_arch`.
///
/// Do not call this at a point where the stack needs to be unwound to run
/// destructors.
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
unsafe extern "C" fn switch_to_guest(ctx: &litebox_common_linux::PtRegs) -> ! {
    core::arch::naked_asm!(
        "switch_to_guest_start:",
        // Set `in_guest` now, then check if there is a pending interrupt. If
        // so, jump to the interrupt handler.
        //
        // If an interrupt arrives after the check, then the signal handler will
        // see that the IP is between `switch_to_guest_start` and
        // `switch_to_guest_end` and will set the `interrupt` and jump to
        // `interrupt_callback`.
        "mov BYTE PTR fs:in_guest@tpoff, 1",
        "cmp BYTE PTR fs:interrupt@tpoff, 0",
        "jne interrupt_callback",
        // Restore guest context from ctx.
        "mov rsp, rdi",
        // Switch to the guest fsbase
        "mov rdx, fs:guest_fsbase@tpoff",
        "wrfsbase rdx",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rax",
        "pop rcx",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "add rsp, 8",           // skip orig_rax
        "pop gs:scratch@tpoff", // read rip into scratch
        "add rsp, 8",           // skip cs
        "popfq",
        "pop rsp",
        "jmp gs:scratch@tpoff", // jump to the guest
        "switch_to_guest_end:",
    );
}

/// AArch64 equivalent of `switch_to_guest` above.
///
/// # Safety
/// Same contract as the x86_64 version; `scratch` must be this thread's own
/// [`Aarch64ThreadScratch`], the same one `run_thread_arch` set up.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
unsafe extern "C" fn switch_to_guest(
    ctx: &litebox_common_linux::PtRegs,
    scratch: *mut Aarch64ThreadScratch,
) -> ! {
    core::arch::naked_asm!(
        "switch_to_guest_start:",
        // Set in_guest now, then check for a pending interrupt -- mirrors x86_64's ordering
        // and comment: if an interrupt arrives after this check, the signal handler sees the
        // PC is between switch_to_guest_start/_end and routes to interrupt_callback itself.
        "mov w9, #1",
        "strb w9, [x1, #48]",       // scratch->in_guest = 1
        "ldrb w9, [x1, #49]",       // scratch->interrupt
        "cbnz w9, interrupt_callback",
        // Restore the guest's TPIDR_EL0.
        "ldr x9, [x1, #40]",        // scratch->guest_tpidr
        "msr tpidr_el0, x9",
        // Reload the guest register file from ctx (x0), matching syscall_callback's dump
        // layout: regs[0..31), sp, pc, pstate, orig_x0, syscallno, unused2.
        "ldp x2, x3, [x0, #16]",
        "ldp x4, x5, [x0, #32]",
        "ldp x6, x7, [x0, #48]",
        "ldp x8, x9, [x0, #64]",
        "ldp x10, x11, [x0, #80]",
        "ldp x12, x13, [x0, #96]",
        "ldp x14, x15, [x0, #112]",
        "ldp x16, x17, [x0, #128]",
        "ldp x18, x19, [x0, #144]",
        "ldp x20, x21, [x0, #160]",
        "ldp x22, x23, [x0, #176]",
        "ldp x24, x25, [x0, #192]",
        "ldp x26, x27, [x0, #208]",
        "ldp x28, x29, [x0, #224]",
        "ldr x30, [x0, #240]",       // regs[30]
        "ldr x1, [x0, #248]",        // PtRegs.sp -> stash in x1 temporarily
        "mov sp, x1",
        "ldr x1, [x0, #256]",        // PtRegs.pc -> the guest branch target
        // Reload x0/x1 last (x0 was our own `ctx` argument, x1 is now the branch target).
        "ldr x9, [x0]",              // regs[0]
        "mov x0, x9",
        "br x1",
        "switch_to_guest_end:",
    );
}

/// Non-guest threads (e.g., network workers, background tasks) should call this
/// function at the start of their execution so the kernel only delivers
/// `SIGALRM` / `SIGINT` to guest threads, which have the proper signal-handler
/// context to re-enter the shim.
fn block_guest_signals() {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        libc::sigaddset(&raw mut set, libc::SIGALRM);
        libc::sigaddset(&raw mut set, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut());
    }
}

/// Spawn a non-guest ("host") thread that automatically blocks guest interrupt
/// signals before running `f`.
///
/// Every background thread created by a runner (network workers, I/O helpers,
/// etc.) should use this function instead of [`std::thread::spawn`] to ensure
/// that `SIGALRM` and `SIGINT` are only delivered to guest threads.
///
/// # Example
///
/// ```ignore
/// let handle = litebox_platform_linux_userland::spawn_host_thread(move || {
///     // This thread will never receive SIGALRM or SIGINT.
///     do_background_work();
/// });
/// ```
pub fn spawn_host_thread<F, T>(f: F) -> std::thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        block_guest_signals();
        f()
    })
}

fn thread_start(
    init_thread: Box<
        dyn litebox::shim::InitThread<ExecutionContext = litebox_common_linux::PtRegs>,
    >,
    mut ctx: litebox_common_linux::PtRegs,
) {
    // Allow caller to run some code before we return to the new thread.
    let shim = init_thread.init();

    run_thread_inner(shim.as_ref(), &mut ctx, false);
    // TODO: have syscall_callback return if we need to terminate the process.
    // We should return this value to the caller so load_program can return it
    // to the user.
}

// A handle to a platform thread.
#[derive(Clone)]
pub struct ThreadHandle(std::sync::Arc<std::sync::Mutex<Option<libc::pthread_t>>>);

thread_local! {
    static CURRENT_THREAD: std::cell::RefCell<Option<ThreadHandle>> = const { std::cell::RefCell::new(None) };
}

impl ThreadHandle {
    /// Runs `f`, ensuring that [`ThreadHandle::current`] can be called within `f`.
    fn run_with_handle<R>(f: impl FnOnce() -> R) -> R {
        let handle = ThreadHandle(std::sync::Arc::new(std::sync::Mutex::new(Some(unsafe {
            libc::pthread_self()
        }))));
        CURRENT_THREAD.with_borrow_mut(|current| {
            assert!(
                current.is_none(),
                "nested with_thread_handle calls are not supported"
            );
            *current = Some(handle);
        });
        let _guard = litebox::utils::defer(|| {
            let current = CURRENT_THREAD.take().unwrap();
            *current.0.lock().unwrap() = None;
        });
        f()
    }

    /// Returns the current thread handle.
    fn current() -> Self {
        CURRENT_THREAD.with_borrow(|thread| {
            thread
                .clone()
                .expect("current_thread called outside of a LiteBox thread")
        })
    }

    /// Interrupts the thread, delivering a signal to it.
    fn interrupt(&self) {
        let thread = self.0.lock().unwrap();
        if let Some(&thread) = thread.as_ref() {
            unsafe {
                libc::pthread_kill(thread, INTERRUPT_SIGNAL_NUMBER.load(Ordering::Relaxed));
            }
        }
    }
}

impl litebox::platform::ThreadProvider for LinuxUserland {
    type ExecutionContext = litebox_common_linux::PtRegs;
    type ThreadSpawnError = std::io::Error;
    type ThreadHandle = ThreadHandle;

    unsafe fn spawn_thread(
        &self,
        ctx: &litebox_common_linux::PtRegs,
        init_thread: Box<
            dyn litebox::shim::InitThread<ExecutionContext = litebox_common_linux::PtRegs>,
        >,
    ) -> Result<(), Self::ThreadSpawnError> {
        let ctx = ctx.clone();
        // TODO: do we need to wait for the handle in the main thread?
        let _handle = std::thread::Builder::new().spawn(move || thread_start(init_thread, ctx))?;

        Ok(())
    }

    fn current_thread(&self) -> Self::ThreadHandle {
        ThreadHandle::current()
    }

    fn interrupt_thread(&self, thread: &Self::ThreadHandle) {
        thread.interrupt();
    }

    #[cfg(debug_assertions)]
    fn run_test_thread<R>(f: impl FnOnce() -> R) -> R {
        // Sets `gsbase = fsbase` (x86_64) or `fs = gs` (x86) on the current thread
        // to mirror the TLS base used in guest context, so that test threads can use the
        // same TLS access code as guest threads.
        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!(
                "rdfsbase {tmp}",
                "wrgsbase {tmp}",
                tmp = out(reg) _,
                options(nostack, preserves_flags),
            );
        }

        // aarch64 equivalent: register a REAL alt-stack-backed Aarch64ThreadScratch for this
        // thread (the same mechanism a genuine guest thread uses, via with_signal_alt_stack),
        // not just the AARCH64_SCRATCH_PTR thread_local. This matters because
        // interrupt_signal_handler's requeue logic (mirroring x86_64's `gsbase != 0` check)
        // deliberately only recognizes a thread as a valid signal target via the alt-stack
        // lookup (`aarch64_scratch_from_context`, keyed off `uc_stack.ss_sp` + the registry) --
        // a signal delivered `SIGEV_SIGNAL`/process-directed is not guaranteed to land on the
        // thread that armed it, so `record_pending_signal` must only ever accept it on a thread
        // that lookup recognizes, exactly like x86_64's real guest threads; a thread that only
        // has AARCH64_SCRATCH_PTR set (but no registered alt-stack) would be silently invisible
        // to that lookup and drop every signal delivered to it.
        #[cfg(target_arch = "aarch64")]
        {
            return with_signal_alt_stack(|alt_stack_base| {
                let scratch = aarch64_scratch_from_alt_stack_base(alt_stack_base);
                // `run_thread_arch`'s naked-asm prologue is the only other writer of
                // `host_tpidr`, and it never runs for a plain test thread -- without this, it
                // stays zeroed (from the fresh mmap), and `signal_handler_exit_guest`'s
                // `msr tpidr_el0, {host_tpidr}` would then zero out this thread's REAL TLS
                // base, corrupting every subsequent TLS access for the rest of the signal
                // handler (and this thread, until something restores it).
                unsafe {
                    let real_tpidr: usize;
                    core::arch::asm!("mrs {}, tpidr_el0", out(reg) real_tpidr, options(nostack, preserves_flags));
                    (*scratch).host_tpidr = real_tpidr;
                }
                AARCH64_SCRATCH_PTR.set(scratch);
                let result = ThreadHandle::run_with_handle(f);
                AARCH64_SCRATCH_PTR.set(core::ptr::null_mut());
                result
            });
        }

        #[cfg(not(target_arch = "aarch64"))]
        ThreadHandle::run_with_handle(f)
    }
}

impl litebox::platform::TimerProvider for LinuxUserland {
    type TimerHandle = TimerHandle;
    type Signal = litebox_common_linux::signal::Signal;

    fn create_timer(
        &self,
        signal: Self::Signal,
    ) -> Result<Self::TimerHandle, litebox::platform::TimerCreationError> {
        // Create a POSIX per-process timer.  We always deliver via SIGALRM at
        // the kernel level (whose handler is already registered) and encode the
        // *desired* guest signal in `sigev_value.sival_int`.  The signal handler
        // reads `si_value` when `si_code == SI_TIMER` to determine which guest
        // signal to record.
        //
        // `SIGEV_THREAD_ID` (Linux-specific), not `SIGEV_SIGNAL`, targets this exact thread
        // (via its real kernel TID) deterministically -- plain `SIGEV_SIGNAL` is
        // process-directed and Linux explicitly does not guarantee it lands on the thread that
        // armed the timer (`signal(7)`: delivered to "any one of the threads that does not
        // currently have the signal blocked"), which is a real, live-reproduced race in a
        // multi-threaded process (e.g. every guest thread running concurrently) that only
        // `interrupt_signal_handler`'s own thread-agnostic-guest requeue fallback (asymmetric
        // in cost, and on aarch64 specifically vulnerable to spinning if the requeue never
        // converges) was previously covering for.
        let mut sev: libc::sigevent = unsafe { core::mem::zeroed() };
        sev.sigev_notify = libc::SIGEV_THREAD_ID;
        sev.sigev_signo = libc::SIGALRM;
        sev.sigev_notify_thread_id = unsafe { libc::gettid() };
        sev.sigev_value.sival_ptr = signal.as_i32() as *mut libc::c_void;

        let mut timer_id: libc::timer_t = std::ptr::null_mut();
        let ret =
            unsafe { libc::timer_create(libc::CLOCK_MONOTONIC, &raw mut sev, &raw mut timer_id) };
        assert!(
            ret == 0,
            "timer_create failed: {}",
            std::io::Error::last_os_error()
        );

        Ok(TimerHandle(timer_id))
    }
}

/// A timer handle backed by POSIX `timer_create` / `timer_settime`.
///
/// Each handle owns an independent kernel timer, so multiple timers can
/// coexist without interfering with each other.
pub struct TimerHandle(libc::timer_t);

// Safety: `timer_t` is an opaque kernel handle safe to send across threads.
unsafe impl Send for TimerHandle {}
unsafe impl Sync for TimerHandle {}

impl Drop for TimerHandle {
    fn drop(&mut self) {
        // Safety: we own the timer and it will not be used after drop.
        unsafe {
            libc::timer_delete(self.0);
        }
    }
}

impl litebox::platform::TimerHandle for TimerHandle {
    fn set_timer(&self, duration: core::time::Duration) {
        let its = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: duration.as_secs().cast_signed().trunc(),
                tv_nsec: duration.subsec_nanos().cast_signed().into(),
            },
        };
        // Safety: valid timer id and itimerspec.
        let ret = unsafe { libc::timer_settime(self.0, 0, &raw const its, std::ptr::null_mut()) };
        assert!(
            ret == 0,
            "timer_settime failed: {}",
            std::io::Error::last_os_error()
        );
    }
}

impl litebox::platform::RawMutexProvider for LinuxUserland {
    type RawMutex = RawMutex;

    fn update_waker(&self, waker: Option<litebox::event::wait::Waker<Self>>)
    where
        Self: litebox::sync::RawSyncPrimitivesProvider,
    {
        let waker_ptr = waker.map_or(std::ptr::null_mut(), |w| Box::into_raw(Box::new(w)));
        #[cfg(target_arch = "x86_64")]
        let mut waker_ptr = waker_ptr;
        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!(
                concat!("xchg ", tls!("wait_waker_addr"), ", {}"),
                inout(reg) waker_ptr,
                options(nostack),
            );
        }
        // SAFETY: `record_pending_signal` (the only reader of `wait_waker_addr` besides this
        // function) runs strictly on this same thread's execution -- see
        // `take_pending_host_signals`'s doc comment for the identical same-thread argument.
        #[cfg(target_arch = "aarch64")]
        let waker_ptr = {
            let scratch_ptr = aarch64_scratch_or_host_only();
            unsafe {
                let old = core::ptr::read_volatile(&raw const (*scratch_ptr).wait_waker_addr);
                core::ptr::write_volatile(&raw mut (*scratch_ptr).wait_waker_addr, waker_ptr as usize);
                old as *mut litebox::event::wait::Waker<Self>
            }
        };
        if !waker_ptr.is_null() {
            // SAFETY: old waker_ptr was created by Box::into_raw in a previous call to update_waker.
            unsafe { drop(Box::from_raw(waker_ptr)) };
        }
    }
}

pub struct RawMutex {
    // The `inner` is the value shown to the outside world as an underlying atomic.
    inner: AtomicU32,
}

impl RawMutex {
    const fn new() -> Self {
        Self {
            inner: AtomicU32::new(0),
        }
    }

    fn block_or_maybe_timeout(
        &self,
        val: u32,
        timeout: Option<Duration>,
    ) -> Result<UnblockedOrTimedOut, ImmediatelyWokenUp> {
        // We wait on the futex, with a timeout if needed
        match futex_timeout(
            &self.inner,
            FutexOperation::Wait,
            /* expected value */ val,
            timeout,
            /* ignored */ None,
        ) {
            Ok(0) | Err(syscalls::Errno::EINTR) => Ok(UnblockedOrTimedOut::Unblocked),
            Err(syscalls::Errno::EAGAIN) => Err(ImmediatelyWokenUp),
            Err(syscalls::Errno::ETIMEDOUT) => Ok(UnblockedOrTimedOut::TimedOut),
            Err(e) => {
                panic!("Unexpected errno={e} for FUTEX_WAIT")
            }
            _ => unreachable!(),
        }
    }
}

impl litebox::platform::RawMutex for RawMutex {
    const INIT: Self = Self::new();

    fn underlying_atomic(&self) -> &AtomicU32 {
        &self.inner
    }

    fn wake_many(&self, n: usize) -> usize {
        assert!(n > 0);
        let n: u32 = n.try_into().unwrap();

        futex_val2(
            &self.inner,
            FutexOperation::Wake,
            /* number to wake up */ n,
            /* val2: ignored */ 0,
            /* uaddr2: ignored */ None,
        )
        .expect("failed to wake up waiters")
    }

    fn block(&self, val: u32) -> Result<(), ImmediatelyWokenUp> {
        match self.block_or_maybe_timeout(val, None) {
            Ok(UnblockedOrTimedOut::Unblocked) => Ok(()),
            Ok(UnblockedOrTimedOut::TimedOut) => unreachable!(),
            Err(ImmediatelyWokenUp) => Err(ImmediatelyWokenUp),
        }
    }

    fn block_or_timeout(
        &self,
        val: u32,
        timeout: Duration,
    ) -> Result<UnblockedOrTimedOut, ImmediatelyWokenUp> {
        self.block_or_maybe_timeout(val, Some(timeout))
    }
}

impl litebox::platform::IPInterfaceProvider for LinuxUserland {
    fn send_ip_packet(&self, packet: &[u8]) -> Result<(), litebox::platform::SendError> {
        let tun_fd = self.tun_socket_fd.read().unwrap();
        let Some(tun_socket_fd) = tun_fd.as_ref() else {
            unimplemented!("networking without tun is unimplemented")
        };
        match unsafe {
            syscalls::syscall3(
                syscalls::Sysno::write,
                usize::try_from(tun_socket_fd.as_raw_fd()).unwrap(),
                packet.as_ptr() as usize,
                packet.len(),
            )
        } {
            Ok(n) => {
                if n != packet.len() {
                    unimplemented!("unexpected size {n}")
                }
                Ok(())
            }
            Err(errno) => {
                unimplemented!("unexpected error {errno}")
            }
        }
    }

    fn receive_ip_packet(
        &self,
        packet: &mut [u8],
    ) -> Result<usize, litebox::platform::ReceiveError> {
        let tun_fd = self.tun_socket_fd.read().unwrap();
        let Some(tun_socket_fd) = tun_fd.as_ref() else {
            unimplemented!("networking without tun is unimplemented")
        };
        unsafe {
            syscalls::syscall3(
                syscalls::Sysno::read,
                usize::try_from(tun_socket_fd.as_raw_fd()).unwrap(),
                packet.as_mut_ptr() as usize,
                packet.len(),
            )
        }
        .map_err(|errno| match errno {
            #[allow(unreachable_patterns, reason = "EAGAIN == EWOULDBLOCK")]
            syscalls::Errno::EWOULDBLOCK | syscalls::Errno::EAGAIN => {
                litebox::platform::ReceiveError::WouldBlock
            }
            _ => unimplemented!("unexpected error {errno}"),
        })
    }
}

impl litebox::platform::TimeProvider for LinuxUserland {
    type Instant = Instant;
    type SystemTime = SystemTime;

    fn now(&self) -> Self::Instant {
        let mut t = core::mem::MaybeUninit::<libc::timespec>::uninit();
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, t.as_mut_ptr()) };
        let t = unsafe { t.assume_init() };
        Instant {
            #[cfg_attr(any(target_arch = "x86_64", target_arch = "aarch64"), expect(clippy::useless_conversion))]
            inner: Duration::new(
                t.tv_sec.reinterpret_as_unsigned().into(),
                t.tv_nsec.reinterpret_as_unsigned().trunc(),
            ),
        }
    }

    fn current_time(&self) -> Self::SystemTime {
        let mut t = core::mem::MaybeUninit::<libc::timespec>::uninit();
        unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, t.as_mut_ptr()) };
        let t = unsafe { t.assume_init() };
        SystemTime {
            #[cfg_attr(any(target_arch = "x86_64", target_arch = "aarch64"), expect(clippy::useless_conversion))]
            inner: Duration::new(
                t.tv_sec.reinterpret_as_unsigned().into(),
                t.tv_nsec.reinterpret_as_unsigned().trunc(),
            ),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant {
    inner: Duration,
}

impl litebox::platform::Instant for Instant {
    fn checked_duration_since(&self, earlier: &Self) -> Option<Duration> {
        self.inner.checked_sub(earlier.inner)
    }
    fn checked_add(&self, duration: core::time::Duration) -> Option<Self> {
        Some(Self {
            inner: self.inner.checked_add(duration)?,
        })
    }
}

pub struct SystemTime {
    inner: Duration,
}

impl litebox::platform::SystemTime for SystemTime {
    const UNIX_EPOCH: Self = SystemTime {
        inner: Duration::ZERO,
    };

    fn duration_since(&self, earlier: &Self) -> Result<core::time::Duration, core::time::Duration> {
        self.inner
            .checked_sub(earlier.inner)
            .ok_or_else(|| earlier.inner.checked_sub(self.inner).unwrap())
    }
}

#[cfg(target_arch = "x86_64")]
impl litebox::platform::ArchSpecificProvider for LinuxUserland {
    // We swap gs and fs before and after a syscall, so while handling a guest
    // syscall the guest's fs base is stored in the gs base register; the
    // per-thread `guest_fsbase` slot holds the value that will be programmed
    // into fs base on guest re-entry.
    fn set_arch_specific_register(
        &self,
        reg: &litebox::platform::ArchSpecificRegister,
        val: usize,
    ) -> Result<(), litebox::platform::ArchSpecificError> {
        match reg {
            litebox::platform::ArchSpecificRegister::FsBase => {
                if litebox_common_linux::arch::is_valid_user_fs_base(val) {
                    set_guest_fsbase(val);
                    Ok(())
                } else {
                    Err(litebox::platform::ArchSpecificError::RegisterUnpermittedValue)
                }
            }
            litebox::platform::ArchSpecificRegister::GsBase => {
                // GS base is used internally by this platform to hold the host
                // TLS base across the guest/host fs-gs swap, so it is not
                // directly programmable by the guest.
                Err(litebox::platform::ArchSpecificError::RegisterReserved)
            }
            _ => Err(litebox::platform::ArchSpecificError::RegisterUnsupported),
        }
    }
    fn get_arch_specific_register(
        &self,
        reg: &litebox::platform::ArchSpecificRegister,
    ) -> Result<usize, litebox::platform::ArchSpecificError> {
        match reg {
            litebox::platform::ArchSpecificRegister::FsBase => Ok(get_guest_fsbase()),
            litebox::platform::ArchSpecificRegister::GsBase => {
                // See note above: gs base is reserved for host TLS on this
                // platform and is not exposed to the guest.
                Err(litebox::platform::ArchSpecificError::RegisterReserved)
            }
            _ => Err(litebox::platform::ArchSpecificError::RegisterUnsupported),
        }
    }
}

type UserMutPtr<T> = litebox::platform::common_providers::userspace_pointers::UserMutPtr<
    litebox::platform::common_providers::userspace_pointers::NoValidation,
    T,
>;
type UserConstPtr<T> = litebox::platform::common_providers::userspace_pointers::UserConstPtr<
    litebox::platform::common_providers::userspace_pointers::NoValidation,
    T,
>;
impl litebox::platform::RawPointerProvider for LinuxUserland {
    type RawConstPointer<T: FromBytes> = UserConstPtr<T>;
    type RawMutPointer<T: FromBytes + IntoBytes> = UserMutPtr<T>;
}

/// Operations currently supported by the safer variants of the Linux futex syscall
/// ([`futex_timeout`] and [`futex_val2`]).
#[repr(i32)]
enum FutexOperation {
    Wait = litebox_common_linux::FUTEX_WAIT,
    Wake = litebox_common_linux::FUTEX_WAKE,
}

/// Safer invocation of the Linux futex syscall, with the "timeout" variant of the arguments.
#[expect(clippy::similar_names, reason = "sec/nsec are as needed by libc")]
fn futex_timeout(
    uaddr: &AtomicU32,
    futex_op: FutexOperation,
    val: u32,
    timeout: Option<Duration>,
    uaddr2: Option<&AtomicU32>,
) -> Result<usize, syscalls::Errno> {
    let uaddr: *const AtomicU32 = std::ptr::from_ref(uaddr);
    let futex_op: i32 = futex_op as _;
    let timeout = timeout.map(|t| {
        const TEN_POWER_NINE: u128 = 1_000_000_000;
        let nanos: u128 = t.as_nanos();
        let tv_sec = nanos
            .checked_div(TEN_POWER_NINE)
            .unwrap()
            .try_into()
            .unwrap();
        let tv_nsec = nanos
            .checked_rem(TEN_POWER_NINE)
            .unwrap()
            .try_into()
            .unwrap();
        litebox_common_linux::Timespec { tv_sec, tv_nsec }
    });
    let uaddr2: *const AtomicU32 = uaddr2.map_or(std::ptr::null(), |u| u);
    unsafe {
        syscalls::syscall6(
            syscalls::Sysno::futex,
            uaddr as usize,
            usize::try_from(futex_op).unwrap(),
            val as usize,
            if let Some(t) = timeout.as_ref() {
                core::ptr::from_ref(t) as usize
            } else {
                0 // No timeout
            },
            uaddr2 as usize,
            // argument `val3` is ignored for this futex operation;
            0,
        )
    }
}

/// Safer invocation of the Linux futex syscall, with the "val2" variant of the arguments.
fn futex_val2(
    uaddr: &AtomicU32,
    futex_op: FutexOperation,
    val: u32,
    val2: u32,
    uaddr2: Option<&AtomicU32>,
) -> Result<usize, syscalls::Errno> {
    let uaddr: *const AtomicU32 = std::ptr::from_ref(uaddr);
    let futex_op: i32 = futex_op as _;
    let uaddr2: *const AtomicU32 = uaddr2.map_or(std::ptr::null(), |u| u);
    unsafe {
        syscalls::syscall6(
            syscalls::Sysno::futex,
            uaddr as usize,
            usize::try_from(futex_op).unwrap(),
            val as usize,
            val2 as usize,
            uaddr2 as usize,
            // argument `val3` is ignored for this futex operation;
            0,
        )
    }
}

fn prot_flags(flags: MemoryRegionPermissions) -> ProtFlags {
    let mut res = ProtFlags::PROT_NONE;
    res.set(
        ProtFlags::PROT_READ,
        flags.contains(MemoryRegionPermissions::READ),
    );
    res.set(
        ProtFlags::PROT_WRITE,
        flags.contains(MemoryRegionPermissions::WRITE),
    );
    res.set(
        ProtFlags::PROT_EXEC,
        flags.contains(MemoryRegionPermissions::EXEC),
    );
    if flags.contains(MemoryRegionPermissions::SHARED) {
        unimplemented!()
    }
    res
}

impl<const ALIGN: usize> litebox::platform::PageManagementProvider<ALIGN> for LinuxUserland {
    const TASK_ADDR_MIN: usize = 0x1_0000; // default linux config
    #[cfg(target_arch = "x86_64")]
    const TASK_ADDR_MAX: usize = 0x7FFF_FFFF_F000; // (1 << 47) - PAGE_SIZE;
    #[cfg(target_arch = "aarch64")]
    const TASK_ADDR_MAX: usize = 0xFFFF_FFFF_F000; // (1 << 48) - PAGE_SIZE; matches
    // litebox_common_linux::arch::USER_ADDR_END for the default 48-bit aarch64 Linux VA config.

    // A `memfd_create` file descriptor. Cast to/from `usize` at the trait boundary; the raw fd
    // number is just an opaque per-process kernel-object identifier, safe to copy and pass
    // across threads (the same fd number refers to the same open file description from any
    // thread of this process).
    type SharedMemoryHandle = usize;

    fn allocate_pages(
        &self,
        suggested_range: core::ops::Range<usize>,
        initial_permissions: MemoryRegionPermissions,
        can_grow_down: bool,
        populate_pages_immediately: bool,
        fixed_address_behavior: FixedAddressBehavior,
    ) -> Result<Self::RawMutPointer<u8>, litebox::platform::page_mgmt::AllocationError> {
        let flags = MapFlags::MAP_PRIVATE
            | MapFlags::MAP_ANONYMOUS
            | match fixed_address_behavior {
                FixedAddressBehavior::Hint => MapFlags::empty(),
                FixedAddressBehavior::Replace => MapFlags::MAP_FIXED,
                FixedAddressBehavior::NoReplace => MapFlags::MAP_FIXED_NOREPLACE,
            }
            | if can_grow_down {
                MapFlags::MAP_GROWSDOWN
            } else {
                MapFlags::empty()
            }
            | if populate_pages_immediately {
                MapFlags::MAP_POPULATE
            } else {
                MapFlags::empty()
            };
        let r = unsafe {
            syscalls::syscall6(
                syscalls::Sysno::mmap,
                suggested_range.start,
                suggested_range.len(),
                prot_flags(initial_permissions)
                    .bits()
                    .reinterpret_as_unsigned() as usize,
                flags.bits().reinterpret_as_unsigned() as usize,
                usize::MAX,
                0,
            )
        };
        let ptr = r.map_err(|err| match err {
            syscalls::Errno::ENOMEM => litebox::platform::page_mgmt::AllocationError::OutOfMemory,
            syscalls::Errno::EEXIST => {
                assert!(matches!(
                    fixed_address_behavior,
                    FixedAddressBehavior::NoReplace
                ));
                litebox::platform::page_mgmt::AllocationError::AddressInUse
            }
            _ => panic!("unhandled mmap error {err}"),
        })?;
        Ok(UserMutPtr::from_usize(ptr))
    }

    unsafe fn deallocate_pages(
        &self,
        range: core::ops::Range<usize>,
    ) -> Result<(), litebox::platform::page_mgmt::DeallocationError> {
        let _ = unsafe { syscalls::syscall2(syscalls::Sysno::munmap, range.start, range.len()) }
            .expect("munmap failed");
        Ok(())
    }

    unsafe fn remap_pages(
        &self,
        old_range: core::ops::Range<usize>,
        new_range: core::ops::Range<usize>,
        _permissions: MemoryRegionPermissions,
    ) -> Result<Self::RawMutPointer<u8>, litebox::platform::page_mgmt::RemapError> {
        let res = unsafe {
            syscalls::syscall5(
                syscalls::Sysno::mremap,
                old_range.start,
                old_range.len(),
                new_range.len(),
                MRemapFlags::MREMAP_MAYMOVE.bits() as usize,
                new_range.start,
            )
            .expect("mremap failed")
        };
        Ok(UserMutPtr::from_usize(res))
    }

    unsafe fn update_permissions(
        &self,
        range: core::ops::Range<usize>,
        new_permissions: MemoryRegionPermissions,
    ) -> Result<(), litebox::platform::page_mgmt::PermissionUpdateError> {
        unsafe {
            syscalls::syscall3(
                syscalls::Sysno::mprotect,
                range.start,
                range.len(),
                prot_flags(new_permissions).bits().reinterpret_as_unsigned() as usize,
            )
        }
        .expect("mprotect failed");
        Ok(())
    }

    fn reserved_pages(&self) -> impl Iterator<Item = &core::ops::Range<usize>> {
        self.reserved_pages.iter()
    }

    fn try_allocate_cow_pages(
        &self,
        suggested_start: usize,
        source_data: &'static [u8],
        permissions: MemoryRegionPermissions,
        fixed_address_behavior: FixedAddressBehavior,
    ) -> Result<Self::RawMutPointer<u8>, CowAllocationError> {
        let Some((file_path, file_offset)) = self.lookup_cow_region(source_data) else {
            return Err(CowAllocationError::UnsupportedSourceRegion);
        };
        if !file_offset.is_multiple_of(ALIGN) {
            return Err(CowAllocationError::Unaligned);
        }

        let file_path_cstr =
            std::ffi::CString::new(file_path.as_os_str().as_encoded_bytes()).unwrap();
        // TODO(jb): We should likely be storing pre-opened FDs, right?
        let fd = unsafe {
            raw_open(
                file_path_cstr.as_ptr() as usize,
                OFlags::RDONLY.bits() as usize,
                0,
            )
        };
        let fd = fd.expect("file should remain unchanged on host");

        let mut flags = MapFlags::MAP_PRIVATE;
        match fixed_address_behavior {
            FixedAddressBehavior::Hint => {}
            FixedAddressBehavior::Replace => flags |= MapFlags::MAP_FIXED,
            FixedAddressBehavior::NoReplace => flags |= MapFlags::MAP_FIXED_NOREPLACE,
        }

        let result = unsafe {
            syscalls::syscall6(
                syscalls::Sysno::mmap,
                suggested_start,
                source_data.len(),
                prot_flags(permissions).bits().reinterpret_as_unsigned() as usize,
                flags.bits().reinterpret_as_unsigned() as usize,
                fd,
                file_offset,
            )
        };

        let _ = unsafe { syscalls::syscall1(syscalls::Sysno::close, fd) };

        match result {
            Ok(ptr) => Ok(UserMutPtr::from_usize(ptr)),
            Err(_) => Err(CowAllocationError::InternalFailure),
        }
    }

    fn create_shared_memory(
        &self,
        size: usize,
    ) -> Result<Self::SharedMemoryHandle, SharedMemoryError> {
        let name = c"litebox-shared-mem";
        let fd =
            unsafe { syscalls::syscall2(syscalls::Sysno::memfd_create, name.as_ptr() as usize, 0) }
                .map_err(|_| SharedMemoryError::OutOfMemory)?;
        if unsafe { syscalls::syscall2(syscalls::Sysno::ftruncate, fd, size) }.is_err() {
            let _ = unsafe { syscalls::syscall1(syscalls::Sysno::close, fd) };
            return Err(SharedMemoryError::OutOfMemory);
        }
        Ok(fd)
    }

    fn map_shared_memory(
        &self,
        handle: Self::SharedMemoryHandle,
        suggested_range: core::ops::Range<usize>,
        initial_permissions: MemoryRegionPermissions,
        fixed_address_behavior: FixedAddressBehavior,
    ) -> Result<Self::RawMutPointer<u8>, SharedMemoryError> {
        let mut flags = MapFlags::MAP_SHARED;
        match fixed_address_behavior {
            FixedAddressBehavior::Hint => {}
            FixedAddressBehavior::Replace => flags |= MapFlags::MAP_FIXED,
            FixedAddressBehavior::NoReplace => flags |= MapFlags::MAP_FIXED_NOREPLACE,
        }

        let result = unsafe {
            syscalls::syscall6(
                syscalls::Sysno::mmap,
                suggested_range.start,
                suggested_range.len(),
                prot_flags(initial_permissions)
                    .bits()
                    .reinterpret_as_unsigned() as usize,
                flags.bits().reinterpret_as_unsigned() as usize,
                handle,
                0,
            )
        };
        match result {
            Ok(ptr) => Ok(UserMutPtr::from_usize(ptr)),
            Err(syscalls::Errno::EEXIST) => Err(SharedMemoryError::AddressInUse),
            Err(_) => Err(SharedMemoryError::OutOfMemory),
        }
    }

    unsafe fn unmap_shared_memory(
        &self,
        range: core::ops::Range<usize>,
    ) -> Result<(), SharedMemoryError> {
        unsafe { syscalls::syscall2(syscalls::Sysno::munmap, range.start, range.len()) }
            .map_err(|_| SharedMemoryError::Unaligned)?;
        Ok(())
    }

    fn close_shared_memory(
        &self,
        handle: Self::SharedMemoryHandle,
    ) -> Result<(), SharedMemoryError> {
        let _ = unsafe { syscalls::syscall1(syscalls::Sysno::close, handle) };
        Ok(())
    }
}

impl litebox::platform::StdioProvider for LinuxUserland {
    fn read_from_stdin(&self, buf: &mut [u8]) -> Result<usize, litebox::platform::StdioReadError> {
        unsafe {
            syscalls::syscall3(
                syscalls::Sysno::read,
                usize::try_from(litebox_common_linux::STDIN_FILENO).unwrap(),
                buf.as_ptr() as usize,
                buf.len(),
            )
        }
        .map_err(|err| match err {
            syscalls::Errno::EPIPE => litebox::platform::StdioReadError::Closed,
            _ => panic!("unhandled error {err}"),
        })
    }

    fn write_to(
        &self,
        stream: litebox::platform::StdioOutStream,
        buf: &[u8],
    ) -> Result<usize, litebox::platform::StdioWriteError> {
        unsafe {
            syscalls::syscall3(
                syscalls::Sysno::write,
                usize::try_from(match stream {
                    litebox::platform::StdioOutStream::Stdout => {
                        litebox_common_linux::STDOUT_FILENO
                    }
                    litebox::platform::StdioOutStream::Stderr => {
                        litebox_common_linux::STDERR_FILENO
                    }
                })
                .unwrap(),
                buf.as_ptr() as usize,
                buf.len(),
            )
        }
        .map_err(|err| match err {
            syscalls::Errno::EPIPE => litebox::platform::StdioWriteError::Closed,
            _ => panic!("unhandled error {err}"),
        })
    }

    fn is_a_tty(&self, stream: litebox::platform::StdioStream) -> bool {
        self.stdio_is_tty[stream as usize]
    }

    fn stdin_ready(&self) -> bool {
        // A real `poll(2)` on the actual inherited stdin fd with a zero timeout: this is the
        // same readiness query the guest-visible `poll`/`select`/`epoll_wait` syscalls need to
        // answer, and on native Linux the host kernel already implements it directly against the
        // real fd -- no emulation required, unlike the Windows console platform where this
        // probe has to be built from scratch (see that platform's `stdin_ready` doc comment).
        #[repr(C)]
        struct PollFd {
            fd: i32,
            events: i16,
            revents: i16,
        }
        const POLLIN: i16 = 0x0001;

        let mut pfd = PollFd {
            fd: litebox_common_linux::STDIN_FILENO,
            events: POLLIN,
            revents: 0,
        };
        #[cfg(target_arch = "x86_64")]
        let ret = unsafe {
            syscalls::syscall3(
                syscalls::Sysno::poll,
                core::ptr::from_mut(&mut pfd) as usize,
                1,
                0,
            )
        };
        // aarch64 has no `poll` syscall; `ppoll` with a zeroed (non-null but immediate) timeout
        // and no signal mask is the exact non-blocking equivalent.
        #[cfg(target_arch = "aarch64")]
        let ret = unsafe {
            let timeout = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            syscalls::syscall4(
                syscalls::Sysno::ppoll,
                core::ptr::from_mut(&mut pfd) as usize,
                1,
                core::ptr::from_ref(&timeout) as usize,
                0,
            )
        };
        match ret {
            Ok(n) => n > 0,
            Err(_) => true,
        }
    }
}

unsafe extern "C" {
    // Defined in asm blocks above
    fn syscall_callback() -> isize;
    fn exception_callback();
    fn interrupt_callback();
    fn switch_to_guest_start();
    fn switch_to_guest_end();
}

unsafe extern "C-unwind" fn init_handler(thread_ctx: &mut ThreadContext) {
    thread_ctx.call_shim(|shim, ctx| shim.init(ctx));
}

unsafe extern "C-unwind" fn reenter_handler(thread_ctx: &mut ThreadContext) {
    thread_ctx.call_shim(|shim, ctx| shim.reenter(ctx));
}

/// Handles Linux syscalls and dispatches them to LiteBox implementations.
///
/// Returns only if the guest thread is exiting. Otherwise, resumes the guest
/// without returning.
///
/// # Safety
///
/// - The `ctx` pointer must be valid pointer to a `litebox_common_linux::PtRegs` structure.
/// - If any syscall argument is a pointer, it must be valid.
///
/// # Panics
///
/// Unsupported syscalls or arguments would trigger a panic for development
/// purposes.
#[allow(clippy::cast_sign_loss)]
unsafe extern "C-unwind" fn syscall_handler(thread_ctx: &mut ThreadContext) {
    thread_ctx.call_shim(|shim, ctx| shim.syscall(ctx));
}

#[cfg(target_arch = "x86_64")]
extern "C-unwind" fn exception_handler(
    thread_ctx: &mut ThreadContext,
    trapno: usize,
    error: usize,
    cr2: usize,
) {
    let info = litebox::shim::ExceptionInfo {
        exception: litebox::shim::Exception(trapno.try_into().unwrap()),
        error_code: error.try_into().unwrap(),
        cr2,
        kernel_mode: false,
    };
    thread_ctx.call_shim(|shim, ctx| shim.exception(ctx, &info));
}

#[cfg(target_arch = "aarch64")]
extern "C-unwind" fn exception_handler(
    thread_ctx: &mut ThreadContext,
    exception_class: usize,
    fault_address: usize,
    _unused: usize,
) {
    let info = litebox::shim::ExceptionInfo {
        exception: litebox::shim::Exception(exception_class.try_into().unwrap()),
        fault_address,
        esr: 0,
        kernel_mode: false,
    };
    thread_ctx.call_shim(|shim, ctx| shim.exception(ctx, &info));
}

extern "C-unwind" fn interrupt_handler(thread_ctx: &mut ThreadContext) {
    thread_ctx.call_shim(|shim, ctx| shim.interrupt(ctx));
}

/// Calls `f` in order to call into a shim entrypoint.
impl ThreadContext<'_> {
    fn call_shim(
        &mut self,
        f: impl FnOnce(
            &dyn litebox::shim::EnterShim<ExecutionContext = litebox_common_linux::PtRegs>,
            &mut litebox_common_linux::PtRegs,
        ) -> ContinueOperation,
    ) {
        // Clear the interrupt flag before calling the shim, since we've handled it
        // now (by calling into the shim), and it might be set again by the shim
        // before returning.
        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!(
                concat!("mov BYTE PTR ", tls!("interrupt"), ", 0"),
                options(nostack, preserves_flags)
            );
        }
        #[cfg(target_arch = "aarch64")]
        let scratch_ptr = {
            let ptr = AARCH64_SCRATCH_PTR.get();
            debug_assert!(!ptr.is_null(), "call_shim invoked outside a guest thread");
            unsafe { core::ptr::write_volatile(&raw mut (*ptr).interrupt, 0) };
            ptr
        };
        let op = f(self.shim, self.ctx);
        match op {
            #[cfg(target_arch = "x86_64")]
            ContinueOperation::Resume => unsafe { switch_to_guest(self.ctx) },
            #[cfg(target_arch = "aarch64")]
            ContinueOperation::Resume => unsafe { switch_to_guest(self.ctx, scratch_ptr) },
            ContinueOperation::Terminate => {}
        }
    }
}

impl litebox::platform::ForkChildVerificationProvider for LinuxUserland {}

impl litebox::platform::SystemInfoProvider for LinuxUserland {
    fn get_syscall_entry_point(&self) -> usize {
        syscall_callback as *const () as usize
    }

    fn get_vdso_address(&self) -> Option<usize> {
        // Enabling VDSO on x86 causes glibc to not set a restorer in signal
        // handlers, which we do not currently support. Disable VDSO for
        // now.
        //
        // TODO: implement VDSO in the shim, don't try to pass through the
        // platform VDSO.
        None
    }
}

thread_local! {
    // Use `ManuallyDrop` for more efficient TLS accesses, since this is always
    // dropped manually before the thread exits.
    static PLATFORM_TLS: Cell<*mut ()> = const { Cell::new(core::ptr::null_mut()) };
}

/// LinuxUserland platform's thread-local storage implementation.
unsafe impl litebox::platform::ThreadLocalStorageProvider for LinuxUserland {
    fn get_thread_local_storage() -> *mut () {
        PLATFORM_TLS.get()
    }

    unsafe fn replace_thread_local_storage(value: *mut ()) -> *mut () {
        PLATFORM_TLS.replace(value)
    }
}

static mut NEXT_SA: [libc::sigaction; 64] = unsafe { core::mem::zeroed() };
static INTERRUPT_SIGNAL_NUMBER: AtomicI32 = AtomicI32::new(0);

fn register_exception_handlers() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        fn sigaction(sig: i32, sa: Option<&libc::sigaction>, old_sa: &mut libc::sigaction) {
            unsafe {
                let r = libc::sigaction(
                    sig,
                    sa.map_or(std::ptr::null(), |sa| &raw const *sa),
                    &raw mut *old_sa,
                );
                assert!(
                    r >= 0,
                    "failed to query existing signal handler for signal {}: {}",
                    sig,
                    std::io::Error::last_os_error()
                );
            }
        }

        let interrupt_signal = {
            // Find an RT signal number for interrupt handling.
            let sig = (libc::SIGRTMIN()..=libc::SIGRTMAX())
                .find(|&i| {
                    let mut old_sa = unsafe { core::mem::zeroed() };
                    sigaction(i, None, &mut old_sa);
                    old_sa.sa_sigaction == libc::SIG_DFL
                })
                .expect("no available real-time signal for interrupt handling");

            let mut sa: libc::sigaction = unsafe { core::mem::zeroed() };
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
            sa.sa_sigaction = interrupt_signal_handler as *const () as usize;
            let mut old_sa = unsafe { core::mem::zeroed() };
            sigaction(sig, Some(&sa), &mut old_sa);
            assert_eq!(
                old_sa.sa_sigaction,
                libc::SIG_DFL,
                "signal {sig} handler already installed",
            );
            INTERRUPT_SIGNAL_NUMBER.store(sig, Ordering::Relaxed);
            sig
        };

        let exception_signals = &[
            libc::SIGSEGV,
            libc::SIGBUS,
            libc::SIGFPE,
            libc::SIGILL,
            libc::SIGTRAP,
            // We'd like to log forbidden syscalls in debug mode
            #[cfg(debug_assertions)]
            libc::SIGSYS,
        ];
        for &sig in exception_signals {
            unsafe {
                let mut sa: libc::sigaction = core::mem::zeroed();
                sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
                sa.sa_sigaction = exception_signal_handler as *const () as usize;
                // Block the interrupt signal while handling exceptions to avoid
                // saving the exception signal handler state as guest state.
                libc::sigaddset(&raw mut sa.sa_mask, interrupt_signal);
                // Note: the handler could start running before this call even
                // returns, so pass `&mut NEXT_SA` directly.
                sigaction(
                    sig,
                    Some(&sa),
                    &mut NEXT_SA[sig.reinterpret_as_unsigned() as usize],
                );
            }
        }

        // Note that non-guest threads should block these signals, so it always fires on a guest thread.
        let traditional_signals = &[libc::SIGINT, libc::SIGALRM];
        for &sig in traditional_signals {
            unsafe {
                let mut sa: libc::sigaction = core::mem::zeroed();
                sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
                sa.sa_sigaction = interrupt_signal_handler as *const () as usize;
                // Block the interrupt signal while handling signals
                libc::sigaddset(&raw mut sa.sa_mask, interrupt_signal);
                let mut old_sa = core::mem::zeroed();
                sigaction(sig, Some(&sa), &mut old_sa);
                assert_eq!(
                    old_sa.sa_sigaction,
                    libc::SIG_DFL,
                    "signal {sig} handler already installed",
                );
            }
        }
    });
}

/// Extra space reserved before the alternate signal stack's usable region, on aarch64 only, to
/// hold a [`Aarch64ThreadScratch`] -- see that type's doc comment for why this lives here rather
/// than in TLS.
#[cfg(target_arch = "aarch64")]
const AARCH64_SCRATCH_SIZE: usize = 0x1000;

/// Sentinel stamped into [`Aarch64ThreadScratch::magic`] -- see that field's doc comment.
/// Arbitrary but fixed; chosen to be unmistakable in a hex dump/debugger, never a plausible
/// stack/heap address or all-zero/all-one pattern that could arise by coincidence.
#[cfg(target_arch = "aarch64")]
const AARCH64_SCRATCH_MAGIC: u64 = 0xA64_5CA7C_A64_5CA7;

/// A fixed-capacity, lock-free registry of every live guest thread's `mapping_base` address
/// (see [`with_signal_alt_stack`]) -- checked by [`aarch64_scratch_from_context`] BEFORE ever
/// dereferencing a `uc_stack.ss_sp`-derived pointer, since that pointer's provenance cannot be
/// trusted (verified live: a thread that never registered any alt-stack still reports a
/// non-null `ss_sp`/nonzero `ss_size` for its primary stack). Membership is checked via a
/// bounded linear scan over plain atomics -- safe to call from a signal handler, unlike a
/// mutex/lock (which risks self-deadlock if the interrupted code already held it).
///
/// A generous fixed capacity (not a dynamically-growing collection, which would need a lock or
/// a lock-free allocator neither of which are signal-handler-safe) -- exceeding it only means a
/// newly-registered guest thread's alt-stack won't be found this way, which the code degrades
/// out of gracefully (treated as \"not a guest thread\", same as index-not-found).
#[cfg(target_arch = "aarch64")]
const AARCH64_MAX_GUEST_THREADS: usize = 4096;
#[cfg(target_arch = "aarch64")]
static AARCH64_GUEST_ALT_STACK_BASES: [AtomicUsize; AARCH64_MAX_GUEST_THREADS] = {
    const ZERO: AtomicUsize = AtomicUsize::new(0);
    [ZERO; AARCH64_MAX_GUEST_THREADS]
};

/// Registers `mapping_base` as a live guest thread's alt-stack scratch region. Called once from
/// [`with_signal_alt_stack`] right after allocation, before the stack is ever registered or
/// used for real.
#[cfg(target_arch = "aarch64")]
fn aarch64_register_guest_alt_stack(mapping_base: usize) {
    for slot in &AARCH64_GUEST_ALT_STACK_BASES {
        if slot
            .compare_exchange(0, mapping_base, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return;
        }
    }
    // Registry exhausted -- see AARCH64_MAX_GUEST_THREADS's doc comment; this thread's
    // signal-handler-side lookups simply won't find it (same as a never-guest thread), a
    // capacity-exceeded degradation rather than a hard failure.
}

/// Unregisters `mapping_base`, called when a guest thread's alt-stack is torn down.
#[cfg(target_arch = "aarch64")]
fn aarch64_unregister_guest_alt_stack(mapping_base: usize) {
    for slot in &AARCH64_GUEST_ALT_STACK_BASES {
        if slot
            .compare_exchange(mapping_base, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return;
        }
    }
}

/// Returns whether `mapping_base` is a currently-registered guest thread alt-stack -- safe to
/// call from a signal handler (see [`AARCH64_GUEST_ALT_STACK_BASES`]'s doc comment).
#[cfg(target_arch = "aarch64")]
fn aarch64_is_registered_guest_alt_stack(mapping_base: usize) -> bool {
    AARCH64_GUEST_ALT_STACK_BASES
        .iter()
        .any(|slot| slot.load(Ordering::SeqCst) == mapping_base)
}

/// Runs `f` with an alternate signal stack set up. `f` receives the raw base address of the
/// mapping backing that alternate stack (below the guard page, i.e. the very start of the
/// mmap'd region) -- on aarch64, this address is where [`Aarch64ThreadScratch`] lives; on
/// x86_64 it is unused (host/guest scratch state there lives in ordinary `.tbss` TLS instead,
/// reachable via the fs/gs swap).
fn with_signal_alt_stack<R>(f: impl FnOnce(*mut u8) -> R) -> R {
    let alt_stack_size = libc::SIGSTKSZ * 2;
    let guard_page_size = 0x1000;
    #[cfg(target_arch = "aarch64")]
    let extra_size = AARCH64_SCRATCH_SIZE;
    #[cfg(not(target_arch = "aarch64"))]
    let extra_size = 0;
    let mapping_base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            extra_size + guard_page_size + alt_stack_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert!(
        mapping_base != libc::MAP_FAILED,
        "failed to allocate memory for alternate signal stack: {}",
        std::io::Error::last_os_error()
    );
    let _unmap_guard = litebox::utils::defer(|| {
        #[cfg(target_arch = "aarch64")]
        aarch64_unregister_guest_alt_stack(mapping_base as usize);
        let r = unsafe {
            libc::munmap(
                mapping_base,
                extra_size + guard_page_size + alt_stack_size,
            )
        };
        assert!(
            r == 0,
            "failed to free memory for alternate signal stack: {}",
            std::io::Error::last_os_error()
        );
    });

    let stack_base = unsafe { mapping_base.cast::<u8>().add(extra_size).cast::<libc::c_void>() };

    // Stamp the magic sentinel into the scratch struct and register this mapping in the
    // lock-free guest-alt-stack registry, both before this stack is ever registered/used --
    // see `Aarch64ThreadScratch::magic` and `AARCH64_GUEST_ALT_STACK_BASES`'s doc comments for
    // why both checks are required (uc_stack.ss_sp alone is not sufficient evidence of a real
    // guest alt-stack, and dereferencing an address before confirming registry membership
    // risks a second, nested fault).
    #[cfg(target_arch = "aarch64")]
    {
        aarch64_register_guest_alt_stack(mapping_base as usize);
        unsafe {
            (*aarch64_scratch_from_alt_stack_base(mapping_base.cast::<u8>())).magic =
                AARCH64_SCRATCH_MAGIC;
        }
    }

    // Set up a guard page to catch stack overflows.
    let r = unsafe { libc::mprotect(stack_base, guard_page_size, libc::PROT_NONE) };
    assert!(
        r == 0,
        "failed to set guard page for alternate signal stack: {}",
        std::io::Error::last_os_error()
    );

    let alt_stack = libc::stack_t {
        ss_sp: stack_base.cast(),
        ss_flags: 0,
        ss_size: alt_stack_size,
    };
    let mut oss = libc::stack_t {
        ss_sp: std::ptr::null_mut(),
        ss_flags: 0,
        ss_size: 0,
    };
    unsafe {
        let r = libc::sigaltstack(&raw const alt_stack, &raw mut oss);
        assert!(
            r >= 0,
            "failed to set up alternate signal stack: {}",
            std::io::Error::last_os_error(),
        );
    }
    let _restore_guard = litebox::utils::defer(|| unsafe {
        let r = libc::sigaltstack(&raw const oss, std::ptr::null_mut());
        assert!(
            r >= 0,
            "failed to restore original signal stack: {}",
            std::io::Error::last_os_error()
        );
    });
    f(mapping_base.cast::<u8>())
}

/// Called from signal handlers to fix up thread state after potentially running
/// in the guest.
///
/// Restores the proper host `fsbase` so that TLS can be used. Clears `in_guest`
/// and optionally sets `interrupt`. If `in_guest` was previously set, returns
/// the guest context pointer (which does not necessarily have up-to-date guest
/// register state yet).
#[cfg(target_arch = "x86_64")]
#[cfg(target_arch = "x86_64")]
fn signal_handler_exit_guest(
    _context: &libc::ucontext_t,
    set_interrupt: bool,
) -> Option<*mut litebox_common_linux::PtRegs> {
    unsafe {
        let gsbase: u64;
        core::arch::asm! {
            "rdgsbase {}", out(reg) gsbase
        };
        let is_in_guest = if gsbase == 0 {
            false
        } else {
            let in_guest: u8;
            core::arch::asm! {
                "mov {in_guest}, BYTE PTR gs:in_guest@tpoff",
                "mov BYTE PTR gs:in_guest@tpoff, 0",
                in_guest = out(reg_byte) in_guest,
                options(nostack, preserves_flags)
            }
            if set_interrupt {
                core::arch::asm! {
                    "mov BYTE PTR gs:interrupt@tpoff, 1",
                    options(nostack, preserves_flags)
                };
            }
            in_guest != 0
        };
        if !is_in_guest {
            return None;
        }

        let guest_context_top: *mut litebox_common_linux::PtRegs;
        core::arch::asm! {
            "wrfsbase {gsbase}",
            "mov {guest_context_top}, fs:guest_context_top@tpoff",
            gsbase = in(reg) gsbase,
            guest_context_top = out(reg) guest_context_top,
            options(nostack, preserves_flags)
        };
        Some(guest_context_top.sub(1))
    }
}

/// Recovers this signal-handler invocation's [`Aarch64ThreadScratch`] purely from
/// `context.uc_stack.ss_sp` (populated by the kernel from whichever `sigaltstack` was active
/// when the signal was delivered) -- safe to call unconditionally, before `TPIDR_EL0` has been
/// restored to the host's value, since it touches no TLS.
///
/// Returns `None` if this thread never registered a guest alternate signal stack (i.e. it is
/// not a guest thread at all -- mirrors x86_64's `gsbase == 0` check).
#[cfg(target_arch = "aarch64")]
fn aarch64_scratch_from_context(context: &libc::ucontext_t) -> Option<*mut Aarch64ThreadScratch> {
    let alt_stack_sp = context.uc_stack.ss_sp;
    if alt_stack_sp.is_null() {
        return None;
    }
    // `stack_base` (== `ss_sp`, registered with `sigaltstack`) is `mapping_base +
    // AARCH64_SCRATCH_SIZE` -- the guard page lives WITHIN the alt-stack region itself (its
    // first page, via `mprotect(stack_base, guard_page_size, PROT_NONE)`), not as a separate
    // region between the scratch struct and the stack; do not subtract it again here.
    let mapping_base = alt_stack_sp.cast::<u8>().wrapping_sub(AARCH64_SCRATCH_SIZE);
    // `uc_stack.ss_sp` alone is not trustworthy evidence this is really one of our guest
    // alt-stacks -- verified live, a thread that never called `sigaltstack` at all still
    // reports a non-null `ss_sp` (its ordinary primary stack). Check registry membership
    // FIRST (a lock-free address comparison, never a dereference) before ever reading through
    // the derived pointer; only once membership is confirmed is it safe to check the magic
    // sentinel too (defense in depth against a registry/derivation bug).
    if !aarch64_is_registered_guest_alt_stack(mapping_base as usize) {
        return None;
    }
    let scratch = aarch64_scratch_from_alt_stack_base(mapping_base);
    if unsafe { (*scratch).magic } != AARCH64_SCRATCH_MAGIC {
        return None;
    }
    Some(scratch)
}

#[cfg(target_arch = "aarch64")]
fn signal_handler_exit_guest(
    context: &libc::ucontext_t,
    set_interrupt: bool,
) -> Option<*mut litebox_common_linux::PtRegs> {
    let scratch = aarch64_scratch_from_context(context)?;
    // Restore the host's own TPIDR_EL0 first, before touching anything else -- this is the one
    // mandatory first step on every aarch64 signal-handler path (see `Aarch64ThreadScratch`'s
    // doc comment): every subsequent access in this function (Rust TLS via `AARCH64_SCRATCH_PTR`,
    // ordinary heap/stack data) requires a correct host TPIDR_EL0.
    unsafe {
        let host_tpidr = (*scratch).host_tpidr;
        core::arch::asm!("msr tpidr_el0, {}", in(reg) host_tpidr, options(nostack, preserves_flags));
        AARCH64_SCRATCH_PTR.set(scratch);

        let in_guest = (*scratch).in_guest;
        (*scratch).in_guest = 0;
        if set_interrupt {
            (*scratch).interrupt = 1;
        }
        if in_guest == 0 {
            return None;
        }
        let guest_context_top = (*scratch).guest_context_top as *mut litebox_common_linux::PtRegs;
        Some(guest_context_top.sub(1))
    }
}

/// Copies register state from a Linux signal context to a LiteBox PtRegs
/// structure.
#[cfg(target_arch = "x86_64")]
fn copy_signal_context(regs: &mut litebox_common_linux::PtRegs, context: &libc::ucontext_t) {
    let litebox_common_linux::PtRegs {
        r15,
        r14,
        r13,
        r12,
        rbp,
        rbx,
        r11,
        r10,
        r9,
        r8,
        rax,
        rcx,
        rdx,
        rsi,
        rdi,
        orig_rax,
        rip,
        cs: _,
        eflags,
        rsp,
        ss: _,
    } = regs;
    for (reg, sig_reg) in [
        (r15, libc::REG_R15),
        (r14, libc::REG_R14),
        (r13, libc::REG_R13),
        (r12, libc::REG_R12),
        (rbp, libc::REG_RBP),
        (rbx, libc::REG_RBX),
        (r11, libc::REG_R11),
        (r10, libc::REG_R10),
        (r9, libc::REG_R9),
        (r8, libc::REG_R8),
        (rax, libc::REG_RAX),
        (rcx, libc::REG_RCX),
        (rdx, libc::REG_RDX),
        (rsi, libc::REG_RSI),
        (rdi, libc::REG_RDI),
        (rip, libc::REG_RIP),
        (rsp, libc::REG_RSP),
        (eflags, libc::REG_EFL),
    ] {
        *reg = context.uc_mcontext.gregs[sig_reg.reinterpret_as_unsigned() as usize]
            .reinterpret_as_unsigned()
            .trunc();
    }
    *orig_rax = *rax;
}

/// Copies register state from a Linux signal context to a LiteBox PtRegs
/// structure, aarch64 variant.
#[cfg(target_arch = "aarch64")]
fn copy_signal_context(regs: &mut litebox_common_linux::PtRegs, context: &libc::ucontext_t) {
    for (dst, src) in regs.regs.iter_mut().zip(context.uc_mcontext.regs.iter()) {
        *dst = (*src).trunc();
    }
    regs.sp = context.uc_mcontext.sp.trunc();
    regs.pc = context.uc_mcontext.pc.trunc();
    regs.pstate = context.uc_mcontext.pstate;
    regs.orig_x0 = regs.regs[0];
}

/// Updates a Linux signal context to return to `f` with the given arguments.
#[cfg(target_arch = "x86_64")]
fn set_signal_return(
    context: &mut libc::ucontext_t,
    f: unsafe extern "C" fn(),
    p0: isize,
    p1: isize,
    p2: isize,
    p3: isize,
) {
    let sigctx = &mut context.uc_mcontext;
    sigctx.gregs[libc::REG_RIP as usize] = (f as usize).reinterpret_as_signed() as i64;
    sigctx.gregs[libc::REG_RDI as usize] = p0 as i64;
    sigctx.gregs[libc::REG_RSI as usize] = p1 as i64;
    sigctx.gregs[libc::REG_RDX as usize] = p2 as i64;
    sigctx.gregs[libc::REG_RCX as usize] = p3 as i64;
}

/// Updates a Linux signal context to return to `f` with the given arguments, aarch64 variant
/// (args in `x0-x3`, matching the AAPCS64 calling convention `f` itself is called under).
///
/// `f` is always one of `syscall_callback`/`exception_callback`/`interrupt_callback` (labels
/// inside [`run_thread_arch`]'s naked-asm block). Each one's FIRST instruction is `ldr x9, [x3,
/// #16]` (`scratch->host_sp`) -- x3 is read as the scratch pointer immediately, before anything
/// else, to switch onto the host stack; only after that does it reload `x29` from `[x3, #24]`
/// (`scratch->host_fp`) and re-derive `x0`/`x3` from `[x29, #-THREAD_CTX_SAVE_OFFSET]`. So `p3`
/// here (which becomes x3) MUST be the real scratch pointer, not a discardable payload slot --
/// confirmed live: passing p3=0 (garbage x3) segfaulted immediately on this very first `ldr`,
/// while the guest's real x3 at signal-delivery time is whatever the interrupted guest code
/// happened to leave there (guest code runs with a fully guest-owned register file --
/// `switch_to_guest` loads x0-x30 from the guest's `PtRegs` before `br`-ing in -- so it is
/// never safely usable as scratch without this explicit override). `p0`, once inside the
/// callback, is separately overwritten by that same `[x29, #-offset]` reload before the Rust
/// handler runs, so it stays a free/unused slot; only `p3` carries real meaning on this arch.
#[cfg(target_arch = "aarch64")]
#[allow(
    clippy::cast_sign_loss,
    reason = "p0..p3 carry bit patterns (an exception class, a raw address), not signed \
              magnitudes -- reinterpreted, not converted"
)]
fn set_signal_return(
    context: &mut libc::ucontext_t,
    scratch: *const Aarch64ThreadScratch,
    f: unsafe extern "C" fn(),
    p0: isize,
    p1: isize,
    p2: isize,
    p3: isize,
) {
    let _ = p3;
    let sigctx = &mut context.uc_mcontext;
    sigctx.pc = f as usize as u64;
    sigctx.regs[0] = p0 as u64;
    sigctx.regs[1] = p1 as u64;
    sigctx.regs[2] = p2 as u64;
    // x3 = scratch pointer, not `p3` -- see this function's doc comment. All three callbacks'
    // very first instruction reads `[x3, #16]` before touching anything else.
    sigctx.regs[3] = scratch as u64;
}

/// Signal handler for hardware exceptions (SIGSEGV, SIGBUS, SIGFPE, SIGILL, SIGTRAP).
unsafe extern "C" fn exception_signal_handler(
    signum: libc::c_int,
    info: &mut libc::siginfo_t,
    context: &mut libc::ucontext_t,
) {
    // On aarch64 there is no fast-path syscall-rewriting trampoline (unlike x86_64's patched
    // `syscall`->`call` rewrite): every guest `svc #0` is unpatched and always reaches the
    // kernel directly, so this SIGSYS handler is the ONLY guest-syscall interception point on
    // this architecture. A SIGSYS while guest code was actually executing (`in_guest != 0` in
    // this thread's `Aarch64ThreadScratch`, the same discriminator the exception/interrupt
    // paths already use) must be dispatched into the shim's real syscall emulation via
    // `syscall_callback` -- not logged and EINVAL'd -- or every guest syscall not already on
    // the host's own seccomp allow-list would either bypass litebox's sandboxing/emulation
    // entirely (if the host happens to allow it) or spuriously fail (if not), live-verified via
    // dup2/dup3/kill/setitimer/getcwd/umask/unlinkat/openat(write)/fcntl all being silently
    // dropped before this fix. A SIGSYS while HOST code was executing (glibc/std/litebox's own
    // runtime internals, not the guest) still falls through to the existing debug-log+EINVAL
    // path below.
    #[cfg(target_arch = "aarch64")]
    if signum == libc::SIGSYS {
        if let Some(scratch) = aarch64_scratch_from_context(context) {
            if unsafe { (*scratch).in_guest } != 0 {
                // Do NOT call `signal_handler_exit_guest` here: `syscall_callback`'s own asm
                // (its very first instructions) already clears `in_guest` and swaps
                // TPIDR_EL0 guest->host itself, matching its normal fast-path entry. Doing
                // it again here first would make that asm save the ALREADY-host TPIDR_EL0
                // as "guest_tpidr", corrupting the guest's real TLS base on next re-entry.
                // `syscall_callback`'s asm normally reads x30/LR as both the guest's own
                // return-address (into `PtRegs.regs[30]`) AND the resume PC (into `PtRegs.pc`)
                // -- correct for its normal entry via a `bl` from the patched fast-path
                // trampoline, where the CPU sets LR = the instruction after the `bl` (the
                // guest's real return address for ITS OWN caller is separately preserved on
                // the guest's stack per normal AAPCS64 convention, untouched by this). A
                // trapped `svc #0` does NOT touch LR at all: x30 at this point is the guest's
                // real, needed return address for whatever function contains the `svc`, and
                // must NOT be overwritten -- an earlier version of this fix clobbered it with
                // the resume PC instead, which live-reproduced as an infinite loop (the
                // syscall wrapper's own `ret` then jumps to the former resume PC forever,
                // re-executing the same instruction with no further syscalls, matching the
                // 100%-CPU spin-hang observed on mkdirat.c/sigint.c/statx.c/faccessat.c).
                // Stash the correct resume PC (`uc_mcontext.pc`, which the kernel DOES advance
                // past the faulting `svc`) in the scratch struct instead; `syscall_callback`'s
                // asm patches `PtRegs.pc` from there when nonzero, leaving x30/`regs[30]` alone.
                unsafe { (*scratch).svc_resume_pc = context.uc_mcontext.pc as usize };
                // Stash the guest's real x3 (4th syscall arg) too -- `set_signal_return` below
                // unconditionally overwrites `regs[3]` with the scratch pointer itself (every
                // callback's first instruction depends on x3 already being scratch), so the
                // guest's real x3 must be preserved out-of-band the same way as the resume PC.
                unsafe { (*scratch).svc_real_x3 = context.uc_mcontext.regs[3] as usize };
                // Ensure `run_thread_arch` (and therefore `syscall_callback`) is linked in.
                let _ = run_thread_arch as *const () as usize;
                // `set_signal_return` unconditionally overwrites `regs[0..3]` with its p0/p1/p2
                // arguments (needed by `exception_callback`/`interrupt_callback`, whose payload
                // really does travel through those registers) -- but `syscall_callback` does NOT
                // read its dispatch payload from x0-x2 at all; it re-dumps the guest's LIVE
                // x0-x30 (via `sigreturn`-restored `context.uc_mcontext.regs`) as `PtRegs.regs[]`
                // once it's running. Passing p0=p1=p2=0 (as an earlier version of this fix did)
                // clobbers the guest's real x0/x1/x2 syscall arguments with zeros BEFORE that
                // dump ever runs, corrupting every syscall's first three arguments --
                // live-confirmed via `getcwd(buf, 4096)`: the dumped `PtRegs.regs[0]` (buf) and
                // `regs[1]` (size) both read back as 0. Round-tripping the guest's OWN current
                // x0-x2 through as p0-p2 makes this overwrite a no-op.
                let (x0, x1, x2) = (
                    context.uc_mcontext.regs[0] as isize,
                    context.uc_mcontext.regs[1] as isize,
                    context.uc_mcontext.regs[2] as isize,
                );
                set_signal_return(
                    context,
                    scratch,
                    unsafe {
                        core::mem::transmute::<
                            unsafe extern "C" fn() -> isize,
                            unsafe extern "C" fn(),
                        >(syscall_callback)
                    },
                    x0,
                    x1,
                    x2,
                    0,
                );
                return;
            }
        }
        // HOST code (litebox's own runtime, not the guest) invoked one of the seven syscalls
        // removed from the seccomp allow-list above (see `aarch64_syscall_proxy`'s module doc).
        // Forward the real syscall to the dedicated unfiltered proxy thread and write its real
        // result back, so host code sees the same behavior as if the syscall had been allowed
        // through directly -- only reached for these seven numbers; anything else still falls
        // through to the debug-log+EINVAL path below unchanged.
        let sysno = context.uc_mcontext.regs[8].cast_signed();
        const PROXIED: [i64; 7] = [
            libc::SYS_close,
            libc::SYS_dup,
            libc::SYS_clock_gettime,
            libc::SYS_set_tid_address,
            libc::SYS_prlimit64,
            libc::SYS_readlinkat,
            libc::SYS_fstat,
        ];
        if PROXIED.contains(&sysno) {
            let regs = &context.uc_mcontext.regs;
            let args = [
                regs[0] as usize,
                regs[1] as usize,
                regs[2] as usize,
                regs[3] as usize,
                regs[4] as usize,
                regs[5] as usize,
            ];
            let result = aarch64_syscall_proxy::proxy(sysno, args);
            context.uc_mcontext.regs[0] = result as u64;
            return;
        }
    }
    // Return an error code for the syscall and log it in debug mode.
    #[cfg(debug_assertions)]
    if signum == libc::SIGSYS {
        use core::fmt::Write as _;
        #[cfg(target_arch = "x86_64")]
        let (sysno, path_arg) = {
            let eax_idx = libc::REG_RAX as usize;
            let sysno = context.uc_mcontext.gregs[eax_idx];
            context.uc_mcontext.gregs[eax_idx] = i64::from(-libc::EINVAL);
            let rsi = context.uc_mcontext.gregs[libc::REG_RSI as usize] as *const i8;
            (sysno, rsi)
        };
        #[cfg(target_arch = "aarch64")]
        let (sysno, path_arg) = {
            // x8 holds the syscall number on aarch64; the kernel's SIGSYS delivery leaves the
            // trapped registers in uc_mcontext.regs, indexed the same way as PtRegs.regs.
            let sysno = context.uc_mcontext.regs[8].cast_signed();
            // Sign-extend, not zero-extend: the aarch64 syscall-return-value convention (like
            // x86_64's) is a signed 64-bit value in the return register, and `Errno::from_ret`
            // (and the kernel's own `IS_ERR_VALUE`) test the top bits of the full 64-bit word --
            // a zero-extended `-EINVAL` (0x0000_0000_ffff_ffea) reads back as a huge but
            // "successful" positive value instead of the intended error.
            context.uc_mcontext.regs[0] = i64::from(-libc::EINVAL).cast_unsigned();
            let path_arg = context.uc_mcontext.regs[1] as *const core::ffi::c_char;
            (sysno, path_arg)
        };
        // Signal-safe: format on the stack via arrayvec (no heap allocation).
        let mut buf = arrayvec::ArrayString::<320>::new();
        if sysno == libc::SYS_openat {
            let c_path = unsafe { core::ffi::CStr::from_ptr(path_arg) };
            // libc may call `openat` for certain files that we can ignore, e.g., /proc/sys/vm/overcommit_memory.
            // Log the paths in case we need to allow some of them in the future.
            let _ = writeln!(buf, "INFO: openat with {c_path:?} is not allowed");
        } else {
            let _ = writeln!(buf, "WARNING: disallowed syscall invoked: {sysno}");
        }
        let _ = unsafe {
            syscalls::syscall3(
                syscalls::Sysno::write,
                libc::STDERR_FILENO as usize,
                buf.as_ptr() as usize,
                buf.len(),
            )
        };
        return;
    }

    let Some(regs) = signal_handler_exit_guest(context, false) else {
        return unsafe { next_signal_handler(signum, info, context) };
    };
    copy_signal_context(unsafe { &mut *regs }, context);

    // Ensure that `run_thread_arch` is linked in so that `exception_callback` is visible.
    let _ = run_thread_arch as *const () as usize;

    // Jump to exception_callback.
    #[cfg(target_arch = "x86_64")]
    {
        let sigctx = &context.uc_mcontext;
        let (trapno, err, cr2) = (
            sigctx.gregs[libc::REG_TRAPNO as usize].trunc(),
            sigctx.gregs[libc::REG_ERR as usize].trunc(),
            sigctx.gregs[libc::REG_CR2 as usize].trunc(),
        );
        set_signal_return(context, exception_callback, 0, trapno, err, cr2);
    }
    // aarch64's SIGSEGV/SIGBUS/SIGILL/SIGFPE/SIGTRAP delivery carries the exception class and
    // fault address via siginfo/an extended sigcontext record, not fixed mcontext fields the
    // way x86_64's TRAPNO/ERR/CR2 gregs slots do -- `signum` and `info.si_addr()` are the
    // portable, reliable sources here, matched to litebox::shim::Exception's ARM exception
    // classes by `signum` alone (ESR_EL1 itself is not exposed through the standard
    // ucontext_t/siginfo_t ABI, only through the kernel's optional `esr_context` sigcontext
    // extension record, which this handler does not currently walk).
    #[cfg(target_arch = "aarch64")]
    {
        let exception = match signum {
            libc::SIGILL => litebox::shim::Exception::BRK64,
            libc::SIGTRAP => litebox::shim::Exception::BREAKPOINT_CURRENT_EL,
            // SIGSEGV/SIGBUS (and anything else reaching this arm) are both address-fault
            // classes on real hardware; map to a data abort, the more common of the two aborts
            // for a userspace fault (as opposed to an instruction abort on execute-from an
            // unmapped page, which is comparatively rare for the guest workloads this shim
            // targets).
            _ => litebox::shim::Exception::DATA_ABORT_CURRENT_EL,
        };
        let fault_address = unsafe { info.si_addr() } as usize;
        // Safe to unwrap: `signal_handler_exit_guest` above already confirmed `context` maps to
        // a real, registered guest alt-stack (it returned `Some`), which is the same evidence
        // `aarch64_scratch_from_context` re-derives here.
        let scratch = aarch64_scratch_from_context(context).unwrap();
        set_signal_return(
            context,
            scratch,
            exception_callback,
            0,
            isize::from(exception.0),
            fault_address.cast_signed(),
            0,
        );
    }
}

/// Runs the next signal handler in the chain.
unsafe fn next_signal_handler(
    signum: libc::c_int,
    info: &mut libc::siginfo_t,
    context: &mut libc::ucontext_t,
) {
    if signum == libc::SIGSEGV {
        let ip: usize = {
            #[cfg(target_arch = "x86_64")]
            {
                context.uc_mcontext.gregs[libc::REG_RIP as usize]
                    .reinterpret_as_unsigned()
                    .trunc()
            }
            #[cfg(target_arch = "aarch64")]
            {
                context.uc_mcontext.pc.trunc()
            }
        };
        if let Some(fixup_addr) = litebox::mm::exception_table::search_exception_tables(ip) {
            #[cfg(target_arch = "x86_64")]
            {
                context.uc_mcontext.gregs[libc::REG_RIP as usize] =
                    fixup_addr.reinterpret_as_signed() as i64;
            }
            #[cfg(target_arch = "aarch64")]
            {
                context.uc_mcontext.pc = fixup_addr as u64;
            }
            return;
        }
    }

    unsafe {
        let next_sa = &NEXT_SA[signum.reinterpret_as_unsigned() as usize];
        match next_sa.sa_sigaction {
            libc::SIG_DFL => {
                // Block this signal and raise.
                let mut set: libc::sigset_t = core::mem::zeroed();
                libc::sigemptyset(&raw mut set);
                libc::sigaddset(&raw mut set, signum);
                libc::sigprocmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut());
                libc::raise(signum);
                unreachable!()
            }
            libc::SIG_IGN => {}
            _ => {
                // Call the next handler
                if next_sa.sa_flags & libc::SA_SIGINFO == 0 {
                    let handler: extern "C" fn(libc::c_int) =
                        core::mem::transmute(next_sa.sa_sigaction);
                    handler(signum);
                } else {
                    let handler: extern "C" fn(
                        libc::c_int,
                        *mut libc::siginfo_t,
                        *mut libc::ucontext_t,
                    ) = core::mem::transmute(next_sa.sa_sigaction);
                    handler(signum, info, context);
                }
            }
        }
    }
}

/// Records a pending host signal in the `.tbss` bitmask and wakes any condvar
/// the thread is blocked on.
///
/// # Safety
///
/// Must be called from a signal handler on a guest thread whose saved host TLS
/// segment register is valid.
#[cfg(target_arch = "x86_64")]
unsafe fn record_pending_signal(signal: litebox_common_linux::signal::Signal) {
    let mask: u32 = 1u32 << (signal.as_i32() - 1);
    unsafe {
        core::arch::asm!(
            concat!("lock or DWORD PTR ", saved_tls!("pending_host_signals"), ", {mask:e}"),
            mask = in(reg) mask,
            options(nostack)
        );
    }
    let waker_addr: usize;
    unsafe {
        core::arch::asm!(
            concat!("mov {}, ", saved_tls!("wait_waker_addr")),
            out(reg) waker_addr,
            options(nostack, preserves_flags)
        );
    }
    if waker_addr == 0 {
        return;
    }
    // SAFETY: if `waker_addr` is not zero, that means the current thread is suspended
    // to handle this signal and it points to a valid Waker whose lifetime spans the
    // entire interruptible wait, set by [`RawMutexProvider::update_waker`].
    let waker = unsafe { &*(waker_addr as *const litebox::event::wait::Waker<LinuxUserland>) };
    waker.wake();
}

/// aarch64 variant: `scratch` must be a valid, already-recovered pointer to this thread's
/// [`Aarch64ThreadScratch`] (see [`aarch64_scratch_from_context`]) -- unlike x86_64's version,
/// this does not (and cannot safely) re-derive it, since doing so from inside a signal handler
/// requires the same context-dependent lookup the caller has already performed.
#[cfg(target_arch = "aarch64")]
unsafe fn record_pending_signal(
    scratch: *mut Aarch64ThreadScratch,
    signal: litebox_common_linux::signal::Signal,
) {
    let mask: u32 = 1u32 << (signal.as_i32() - 1);
    // SAFETY: `scratch` is valid per this function's own safety contract; this is the only
    // writer of `pending_host_signals` from a signal-handler context, and `take_pending_host_
    // signals` only ever runs on this thread's own non-signal execution, so no atomic RMW is
    // needed here beyond ordinary same-thread visibility (a signal handler always fully
    // interrupts and returns to the same thread's own execution, per POSIX).
    unsafe {
        (*scratch).pending_host_signals |= mask;
    }
    let waker_addr = unsafe { (*scratch).wait_waker_addr };
    if waker_addr == 0 {
        return;
    }
    // SAFETY: if `waker_addr` is not zero, that means the current thread is suspended
    // to handle this signal and it points to a valid Waker whose lifetime spans the
    // entire interruptible wait, set by [`RawMutexProvider::update_waker`].
    let waker = unsafe { &*(waker_addr as *const litebox::event::wait::Waker<LinuxUserland>) };
    waker.wake();
}

/// Signal handler for interrupt signals.
unsafe fn interrupt_signal_handler(
    signum: libc::c_int,
    info: &mut libc::siginfo_t,
    context: &mut libc::ucontext_t,
) {
    #[cfg(debug_assertions)]
    let raise_signal = |signum: libc::c_int, info: &libc::siginfo_t| {
        // Block the signal on this non-guest thread so the kernel won't
        // deliver it here again, then re-raise as process-directed so a
        // guest thread picks it up.
        //
        // This should only be called by test threads (spawned via cargo test).
        // Other non-guest threads like network worker threads should have already blocked these signals.
        unsafe {
            let mut set: libc::sigset_t = core::mem::zeroed();
            libc::sigemptyset(&raw mut set);
            libc::sigaddset(&raw mut set, signum);
            libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut());
            let val = info.si_value();
            libc::sigqueue(libc::getpid(), signum, val);
        }
    };

    // Record host-originated signals (SIGINT, SIGALRM, etc.) in the
    // per-thread pending bitmask so the shim can forward them to the guest.
    // TODO: no realtime signal support for now.
    if signum > 0 && signum < 32 {
        // For timer-originated signals (and their re-raises via `sigqueue`),
        // the desired guest signal is encoded in `si_value.sival_ptr`
        // (set by `create_timer`).  For other sources (e.g. `kill()`), use
        // the signal number directly.
        let guest_signum = if info.si_code == libc::SI_TIMER || info.si_code == libc::SI_QUEUE {
            unsafe { info.si_value().sival_ptr as libc::c_int }
        } else {
            signum
        };

        // Only record signals that can be forwarded to the guest as
        // litebox_common_linux::signal::Signal. Unknown signals are silently dropped.
        let Ok(signal) = litebox_common_linux::signal::Signal::try_from(guest_signum) else {
            return;
        };

        // Check whether the saved host TLS segment is valid (i.e. this is a
        // guest thread). If not, re-raise the signal process-wide.
        #[cfg(target_arch = "x86_64")]
        {
            let gsbase: u64;
            unsafe { core::arch::asm!("rdgsbase {}", out(reg) gsbase) };
            if gsbase != 0 {
                // SAFETY: we verified the saved host TLS segment is valid above.
                unsafe { record_pending_signal(signal) };
            } else {
                #[cfg(debug_assertions)]
                raise_signal(signum, info);
                return;
            }
        }
        #[cfg(target_arch = "aarch64")]
        {
            // A genuine guest thread's scratch (found via the alt-stack lookup) is the ONLY
            // case that stops the re-queue chain -- mirrors x86_64's `gsbase != 0` check
            // exactly. A host-only thread (this signal landed on some cargo-test worker
            // thread, or a host thread merely waiting via WaitState, neither the actual
            // *guest* thread the signal targets) must still fall through to `raise_signal`'s
            // process-wide re-queue below: `SIGEV_SIGNAL`-delivered timer signals (and `kill()`
            // targeting the process) are NOT guaranteed to land on the specific thread that
            // armed them, so the only correct way to eventually reach the right thread is the
            // same requeue-until-a-guest-thread-catches-it loop x86_64 already relies on.
            // (A host-only thread's own record via `aarch64_scratch_or_host_only` -- used by
            // `update_waker`/`take_pending_host_signals` -- is written/read on that thread's
            // OWN normal execution, never through this signal-handler path.)
            if let Some(scratch) = aarch64_scratch_from_context(context) {
                // SAFETY: `scratch` was just derived from this thread's own active
                // sigaltstack, valid per `aarch64_scratch_from_context`'s contract.
                unsafe { record_pending_signal(scratch, signal) };
            } else {
                #[cfg(debug_assertions)]
                raise_signal(signum, info);
                return;
            }
        }
    }

    // The interrupt signal can arrive in different contexts:
    // 1. The thread is running in the host at the beginning of the syscall
    //    handler. Do nothing--the syscall handler will handle the interrupt.
    // 2. The thread is running in the host, with in_guest = 0. Just record that
    //    an interrupt is pending; it will be checked next time we switch to the
    //    guest.
    // 3. The thread is running in the host, with in_guest = 1, in the middle of
    //    restoring the guest context. We need to jump to the interrupt handler
    //    without overwriting the saved guest context.
    // 4. The thread is running in the guest. We need to save the context and
    //    jump to the interrupt handler.
    //
    // Note that this signal can't arrive while in an exception signal handler
    // since we mask the interrupt signal while handling exceptions.

    #[cfg(target_arch = "x86_64")]
    let ip = context.uc_mcontext.gregs[libc::REG_RIP as usize]
        .reinterpret_as_unsigned()
        .trunc();
    #[cfg(target_arch = "aarch64")]
    let ip = context.uc_mcontext.pc.trunc();

    // Case 1: at the beginning of the syscall handler.
    //
    // FUTURE: handle trampoline code, too. This is somewhat less important
    // because it's probably fine for the shim to observe a guest context that
    // is inside the trampoline.
    if ip == syscall_callback as *const () as usize {
        // No need to clear `in_guest` or set interrupt; the syscall handler will
        // clear `in_guest` and call into the shim.
        return;
    }

    // Clear `in_guest` and set `interrupt`.
    let Some(regs) = signal_handler_exit_guest(context, true) else {
        // Case 2: not in guest.
        return;
    };

    // If the interrupt happened while returning to the guest, don't overwrite
    // the saved context.
    let in_switch_to_guest = (switch_to_guest_start as *const () as usize
        ..switch_to_guest_end as *const () as usize)
        .contains(&ip);
    if in_switch_to_guest {
        // Case 3: in the middle of restoring guest context. Don't overwrite it.
    } else {
        // Case 4: in guest. Copy out the context.
        copy_signal_context(unsafe { &mut *regs }, context);
    }
    // Cases 3 and 4: jump to interrupt handler.
    #[cfg(target_arch = "x86_64")]
    set_signal_return(context, interrupt_callback, 0, 0, 0, 0);
    #[cfg(target_arch = "aarch64")]
    {
        // Safe to unwrap: `signal_handler_exit_guest` above already confirmed `context` maps to
        // a real, registered guest alt-stack (it returned `Some`).
        let scratch = aarch64_scratch_from_context(context).unwrap();
        set_signal_return(context, scratch, interrupt_callback, 0, 0, 0, 0);
    }
}

impl litebox::platform::CrngProvider for LinuxUserland {
    fn fill_bytes_crng(&self, buf: &mut [u8]) {
        getrandom::fill(buf).expect("getrandom failed");
    }
}

impl litebox::platform::DerivedKeyProvider for LinuxUserland {
    fn derive_key<E>(
        &self,
        shim_kdf: Option<fn(&[u8], litebox::platform::KDFParams) -> Result<(), E>>,
        params: litebox::platform::KDFParams,
    ) -> Result<(), litebox::platform::DerivedKeyError<E>> {
        let Some(boot_id) = self.boot_id.get() else {
            return Err(litebox::platform::DerivedKeyError::UnsupportedRebootPersistentKey);
        };
        match shim_kdf {
            None => {
                // TODO: Ideally, we'd use something like argon2 or such here to support more shims,
                // but for now, we just return an error.
                Err(litebox::platform::DerivedKeyError::ShimKDFRequired)
            }
            Some(shim_kdf) => {
                // We trust the shim in this platform, since it is in the same trust boundary as us.
                // Thus (unlike some other platforms) we do not need to manually hide the "key", and
                // can just run the KDF as-is.
                //
                // Our key is actually just the boot ID itself.
                Ok(shim_kdf(boot_id, params)?)
            }
        }
    }
}

/// Dummy `VmapManager`.
///
/// In general, userland platforms do not support `vmap` and `vunmap` (which are kernel functions).
/// We might need to emulate these functions' behaviors using virtual addresses for development or
/// testing, or use a kernel module to provide this functionality (if needed).
unsafe impl<const ALIGN: usize> VmapManager<ALIGN> for LinuxUserland {
    type MapInfo = litebox_common_linux::vmap::NoopPhysPageMapInfo;

    fn validate_unowned(
        &self,
        _pages: &litebox_common_linux::vmap::PhysPageAddrArray<ALIGN>,
    ) -> Result<(), litebox_common_linux::vmap::PhysPointerError> {
        Err(litebox_common_linux::vmap::PhysPointerError::UnsupportedOperation)
    }

    unsafe fn protect(
        &self,
        _pages: &litebox_common_linux::vmap::PhysPageAddrArray<ALIGN>,
        _perms: litebox_common_linux::vmap::PhysPageMapPermissions,
    ) -> Result<(), litebox_common_linux::vmap::PhysPointerError> {
        Err(litebox_common_linux::vmap::PhysPointerError::UnsupportedOperation)
    }
}

/// Dummy `VmemPageFaultHandler`.
///
/// Page faults are handled transparently by the host Linux kernel.
/// Provided to satisfy trait bounds for `PageManager::handle_page_fault`.
impl litebox::mm::linux::VmemPageFaultHandler for LinuxUserland {
    unsafe fn handle_page_fault(
        &self,
        _fault_addr: usize,
        _flags: litebox::mm::linux::VmFlags,
        _error_code: u64,
    ) -> Result<(), litebox::mm::linux::PageFaultError> {
        unreachable!("host kernel handles page faults for Linux userland")
    }

    fn access_error(_error_code: u64, _flags: litebox::mm::linux::VmFlags) -> bool {
        unreachable!("host kernel handles page faults for Linux userland")
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::AtomicU32;
    use std::thread::sleep;

    use litebox::{fs::OFlags, platform::RawMutex};

    use crate::LinuxUserland;
    use litebox::platform::PageManagementProvider;

    extern crate std;

    #[test]
    fn test_raw_mutex() {
        let mutex = std::sync::Arc::new(super::RawMutex {
            inner: AtomicU32::new(0),
        });

        let copied_mutex = mutex.clone();
        std::thread::spawn(move || {
            sleep(core::time::Duration::from_millis(500));
            copied_mutex
                .inner
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            copied_mutex.wake_many(10);
        });

        assert!(mutex.block(0).is_ok());
    }

    #[test]
    fn test_reserved_pages() {
        let platform = LinuxUserland::new(None);
        let reserved_pages: Vec<_> =
            <LinuxUserland as PageManagementProvider<4096>>::reserved_pages(platform).collect();

        // Check that the reserved pages are in order and non-overlapping
        let mut prev = 0;
        for page in reserved_pages {
            assert!(page.start >= prev);
            assert!(page.end > page.start);
            prev = page.end;
        }
    }

    #[test]
    fn test_seccomp_filter() {
        let _platform: &LinuxUserland = LinuxUserland::new(None);
        LinuxUserland::enable_seccomp_filter();

        let pathname = c"/tmp/test_seccomp";
        let mkdir_res = unsafe {
            syscalls::syscall3(
                syscalls::Sysno::mkdirat,
                libc::AT_FDCWD.cast_unsigned() as usize,
                pathname.as_ptr() as usize,
                0o755,
            )
        };
        assert_eq!(
            mkdir_res.unwrap_err(),
            syscalls::Errno::EINVAL,
            "mkdirat should be blocked by seccomp filter"
        );

        let pathname =
            std::ffi::CString::new(format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let open_res = unsafe {
            crate::raw_open(
                pathname.as_ptr() as usize,
                OFlags::RDWR.bits() as usize,
                0,
            )
        };
        assert_eq!(
            open_res.unwrap_err(),
            syscalls::Errno::EINVAL,
            "openat with RDWR should be blocked by seccomp filter"
        );
    }
}
