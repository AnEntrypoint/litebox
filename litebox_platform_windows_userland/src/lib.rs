// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! A [LiteBox platform](../litebox/platform/index.html) for running LiteBox on userland Windows.

// Restrict this crate to only work on Windows. For now, we are restricting this to only x86-64
// Windows, but we _may_ allow for more in the future, if we find it useful to do so.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

mod ctxwatch;
mod fork_verify;
mod net;
pub mod presentation;
pub mod process_fork;
pub mod xproc_sync;

use core::cell::Cell;
use core::panic;
use core::sync::atomic::{AtomicU32, Ordering};
use core::time::Duration;
use std::cell::RefCell;
use std::os::raw::c_void;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::sync::{Arc, Mutex, OnceLock};

use litebox::platform::ImmediatelyWokenUp;
use litebox::platform::UnblockedOrTimedOut;
use litebox::platform::page_mgmt::{
    AllocationError, CowAllocationError, FixedAddressBehavior, MemoryRegionPermissions,
    SharedMemoryError,
};
use litebox::shim::{ContinueOperation, Exception};
use litebox::utils::TruncateExt as _;

use windows_sys::Win32::Foundation::{self as Win32_Foundation, FILETIME};
use windows_sys::Win32::{
    Foundation::GetLastError,
    System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS, EXCEPTION_RECORD,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
    },
    System::Memory::{
        self as Win32_Memory, CreateFileMappingW, MEM_ADDRESS_REQUIREMENTS, MEM_EXTENDED_PARAMETER,
        MEM_EXTENDED_PARAMETER_0, MapViewOfFile3, MemExtendedParameterAddressRequirements,
        PrefetchVirtualMemory, UnmapViewOfFileEx, VirtualAlloc2, VirtualFree, VirtualProtect,
    },
    System::SystemInformation::{self as Win32_SysInfo, GetSystemTimePreciseAsFileTime},
    System::Threading::{self as Win32_Threading, GetCurrentProcess},
    System::WindowsProgramming::QueryUnbiasedInterruptTimePrecise,
};
use zerocopy::{FromBytes, IntoBytes};

extern crate alloc;

// Thread-local storage for FS base state
thread_local! {
    static THREAD_FS_BASE: Cell<usize> = const { Cell::new(0) };
    /// This thread's real Windows TEB pointer (`GS_BASE`), captured once, early, via
    /// [`WindowsUserland::init_thread_gs_base`] -- see that function's doc comment for why a
    /// cached value is needed at all, given `GS_BASE` is normally Windows' own to manage.
    static THREAD_GS_BASE: Cell<usize> = const { Cell::new(0) };
    /// Set while this thread is inside [`vectored_exception_handler`]'s
    /// `diag_fataldump_enabled()` diagnostic block (mallocng `.meta=0` investigation, 2026-08-26).
    /// That block does real work (`Vec`/`String`/`format!`, i.e. host heap allocation) from a VEH
    /// callback; a live investigation pass caught it re-faulting recursively at the SAME
    /// `memmove` instruction 7 times in a row on the exact same thread -- a second exception
    /// raised *while already inside* this diagnostic code re-entering the identical diagnostic
    /// path, obscuring the original guest fault entirely. Per-thread (not a global `AtomicBool`)
    /// because a VEH handler can legitimately run concurrently on unrelated threads and a global
    /// guard would falsely suppress diagnostics for a genuinely separate, simultaneous crash.
    static IN_VEH_DIAG_BLOCK: Cell<bool> = const { Cell::new(false) };
}

/// RAII guard: sets [`IN_VEH_DIAG_BLOCK`] on construction, clears it on drop (including on an
/// early return or a panic unwinding through the diagnostic block), so a second, nested entry
/// into the diagnostic block on the SAME thread can detect it's already running and skip straight
/// past the re-entrant work instead of recursing into the same crash-prone code again.
struct VehDiagBlockGuard {
    already_active: bool,
}

impl VehDiagBlockGuard {
    fn enter() -> Self {
        let already_active = IN_VEH_DIAG_BLOCK.with(Cell::get);
        if !already_active {
            IN_VEH_DIAG_BLOCK.with(|c| c.set(true));
        }
        Self { already_active }
    }
}

impl Drop for VehDiagBlockGuard {
    fn drop(&mut self) {
        if !self.already_active {
            IN_VEH_DIAG_BLOCK.with(|c| c.set(false));
        }
    }
}

/// The userland Windows platform.
///
/// This implements the main [`litebox::platform::Provider`] trait, i.e., implements all platform
/// traits.
pub struct WindowsUserland {
    reserved_pages: alloc::vec::Vec<core::ops::Range<usize>>,
    sys_info: std::sync::RwLock<Win32_SysInfo::SYSTEM_INFO>,
    /// The userspace NAT gateway backing [`IPInterfaceProvider`](litebox::platform::IPInterfaceProvider)
    /// (see the private `net` module), lazily initialized on first network use.
    net_gateway: std::sync::OnceLock<net::NatGateway>,
    /// Backing state for [`read_from_raw_handle`]/[`stdin_ready_raw_handle`]'s console
    /// (`FILE_TYPE_CHAR`) case, lazily initialized (spawning its background reader thread) on
    /// first stdin access. See [`ConsoleStdinReader`]'s doc comment for why this exists. A field
    /// on this per-instance struct rather than a bare `static`, matching `net_gateway` above --
    /// `WindowsUserland::new` always hands back a `&'static Self` in practice, so this is no less
    /// process-lifetime than a bare static would be, without adding to the crate's ratcheted
    /// bare-static count.
    console_stdin_reader: std::sync::OnceLock<ConsoleStdinReader>,
    /// CoW-eligible memory regions, mirroring `litebox_platform_linux_userland::LinuxUserland`'s
    /// identically-shaped field: maps the start address of a registered `'static` host-mmapped
    /// slice to the info needed to re-open its backing file for [`Self::try_allocate_cow_pages`].
    cow_regions: std::sync::RwLock<std::collections::BTreeMap<usize, CowRegionInfo>>,
}

/// Information about a CoW-eligible memory region backed by a file. Mirrors
/// `litebox_platform_linux_userland::CowRegionInfo` exactly.
struct CowRegionInfo {
    /// The path to the backing file on the host filesystem.
    file_path: std::path::PathBuf,
    /// Length of the backing file.
    file_length: usize,
}

impl core::fmt::Debug for WindowsUserland {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WindowsUserland").finish_non_exhaustive()
    }
}

// Safety: Given that SYSTEM_INFO is not Send/Sync (it contains *mut c_void), we use RwLock to
// ensure that the sys_info is only accessed in a thread-safe manner.
// Moreover, SYSTEM_INFO is only initialized once during platform creation, and it is read-only
// after that.
unsafe impl Send for WindowsUserland {}
unsafe impl Sync for WindowsUserland {}

/// Helper functions for managing per-thread FS base
impl WindowsUserland {
    /// Get the current thread's FS base state
    fn get_thread_fs_base() -> usize {
        THREAD_FS_BASE.get()
    }

    /// Set the current thread's FS base
    fn set_thread_fs_base(new_base: usize) {
        THREAD_FS_BASE.set(new_base);
        // Mirror into `TlsState.guest_fs_base` too, if this thread's `TlsState` is already
        // installed -- see that field's doc comment for why the mirror exists. Not yet installed
        // during this platform's own construction (the main thread's very first
        // `init_thread_fs_base()` call, before `install_tls` has ever run for it); harmless to
        // skip the mirror there, since `vectored_exception_handler_entry`'s fast path already
        // falls through to the full handler whenever `get_tls_ptr()` finds nothing installed.
        if let Some(tls) = get_tls_ptr() {
            unsafe { &*tls }.guest_fs_base.set(new_base);
        }
        Self::restore_thread_fs_base();
    }

    /// Restore the current thread's FS base from saved state
    fn restore_thread_fs_base() {
        unsafe {
            litebox_common_linux::wrfsbase(THREAD_FS_BASE.get());
        }
    }

    /// Initialize FS base state for a new thread
    fn init_thread_fs_base() {
        Self::set_thread_fs_base(0);
    }

    /// Captures this thread's real TEB pointer (`GS_BASE`) once, early in the thread's life, so
    /// [`Self::restore_thread_gs_base_if_cleared`] has a known-good value to repair back to later.
    ///
    /// # Why this exists
    ///
    /// `GS_BASE` is normally entirely Windows' own to manage (it always points at this thread's
    /// TEB, e.g. `ntdll`'s own code depends on it) -- litebox never legitimately writes it the
    /// way it does `FS_BASE` (the guest's own TLS base, fully owned and set via `arch_prctl`).
    /// But the exact same "Windows clears this thread's segment-base MSR back to 0 under
    /// scheduling pressure" behavior already documented and repaired for `FS_BASE` (see
    /// `restore_thread_fs_base`'s callers in `vectored_exception_handler`) plausibly affects
    /// `GS_BASE` too -- both are non-standard x86_64 MSRs from the CPU's perspective, and this
    /// platform's own `vectored_exception_handler_entry` fast path (`gs:[r8*8 +
    /// TEB_TLS_SLOTS_OFFSET]`) already depends on `GS_BASE` being correct at one of the hottest,
    /// earliest-in-exception-dispatch code paths in the whole crate. Investigated live while
    /// chasing a reliably reproducible `EXCEPTION_ACCESS_VIOLATION` INSIDE `ntdll.dll` itself
    /// (`is_in_guest=false`, a NULL-pointer read, looping forever at the identical instruction
    /// under nested `vfork()`'s added kernel-transition pressure) -- this repair alone did not
    /// resolve that specific crash (its true cause is still open, see FINDINGS.txt), but is a
    /// real, independently-justified defense against the documented FS_BASE-reset behavior
    /// plausibly extending to `GS_BASE`, confirmed harmless (no regression across repeated runs
    /// of the existing single-`vfork()` repro) and kept on that basis.
    fn init_thread_gs_base() {
        let gs_base = unsafe { litebox_common_linux::rdgsbase() };
        THREAD_GS_BASE.set(gs_base);
    }

    /// Restores this thread's `GS_BASE` from the value [`Self::init_thread_gs_base`] captured, if
    /// the CPU currently reads back a cleared (`0`) value. A no-op if `init_thread_gs_base` was
    /// never called on this thread (`THREAD_GS_BASE` still `0`) -- matches
    /// `restore_thread_fs_base`'s own "never repair to a value we don't actually trust"
    /// discipline.
    fn restore_thread_gs_base_if_cleared() {
        let saved = THREAD_GS_BASE.get();
        if saved != 0 && unsafe { litebox_common_linux::rdgsbase() } == 0 {
            unsafe { litebox_common_linux::wrgsbase(saved) };
        }
    }
}

/// Every diagnostic gate consulted by code reachable from the vectored exception handler,
/// resolved ONCE before any guest thread exists.
///
/// # Why this is a correctness requirement, not a cache
///
/// On Windows `std::env::var_os` is not a cheap read: it allocates
/// (`std::sys::pal::windows::to_u16s` grows a `Vec`, then an `OsString`) and it enters ntdll's
/// process-wide environment critical section via `RtlQueryEnvironmentVariable`. Neither is safe
/// from inside a vectored exception handler, which runs on a thread whose `rsp` is still
/// guest-address memory, at an arbitrary instruction boundary, possibly while another thread holds
/// the environment or heap lock.
///
/// [`ThreadHandle::interrupt`] already learned this the hard way -- see `diag_interrupt_enabled`'s
/// history, a full MATE session frozen with one thread parked in `RtlQueryEnvironmentVariable`
/// and six queued behind it -- but the fix there was applied to that one call site. It was not
/// the only one. `fork_verify::on_single_step` consulted `LITEBOX_VEH_TRACE` and
/// `LITEBOX_DIAG_ALLOC_VEC` on EVERY single-step trap, and verification single-steps the guest
/// instruction by instruction; `mate-session` died reproducibly with an access violation inside
/// `on_single_step` itself, `to_u16s` on the stack and `rax=0xc0000100`
/// (`STATUS_VARIABLE_NOT_FOUND`) -- the environment lookup, faulting.
///
/// Per-call-site caching would leave the same hole, just rarer: a lazily-initialised cache still
/// performs its one real lookup wherever it is first reached, which for a single-step gate is
/// inside the handler. So every gate is read here instead, from
/// [`WindowsUserland::new`], before any guest code runs -- after which the handler only ever reads
/// already-initialised memory.
///
/// One `static` holding every gate, rather than one per gate: `dev_tests/src/ratchet.rs`'s
/// `ratchet_globals` tracks this crate's bare-static count and is trying to reduce it. This
/// replaces `diag_interrupt_enabled`'s own `OnceLock` and two per-thread caches, so the count goes
/// down.
#[derive(Debug)]
pub(crate) struct VehGates {
    /// `LITEBOX_VEH_TRACE`
    pub(crate) veh_trace: bool,
    /// `LITEBOX_DIAG_WAIT4GATE`
    pub(crate) rip0: bool,
    /// `LITEBOX_DIAG_FATALDUMP`
    pub(crate) fataldump: bool,
    /// `LITEBOX_DIAG_FAULT_VQ`
    pub(crate) fault_vq: bool,
    /// `LITEBOX_DIAG_FAULT_MODULE`
    pub(crate) fault_module: bool,
    /// `LITEBOX_DIAG_ALLOC_VEC`
    pub(crate) alloc_vec: bool,
    /// `LITEBOX_DIAG_AVFULL`
    pub(crate) avfull: bool,
    /// `LITEBOX_DIAG_ALLOW_WER`
    pub(crate) allow_wer: bool,
    /// `LITEBOX_DIAG_INTERRUPT`
    pub(crate) interrupt: bool,
    /// `LITEBOX_CODEWATCH`
    pub(crate) codewatch: bool,
    /// `LITEBOX_CODEWATCH=selftest`
    pub(crate) codewatch_selftest: bool,
    /// `LITEBOX_DIAG_WATCHADDR`, already parsed -- so even the parse never runs in the handler.
    pub(crate) watchaddr: Option<usize>,
    /// `LITEBOX_FORKVERIFY_OFF`
    pub(crate) forkverify_off: bool,
}

impl VehGates {
    fn read_environment() -> Self {
        let codewatch = std::env::var_os("LITEBOX_CODEWATCH");
        Self {
            veh_trace: std::env::var_os("LITEBOX_VEH_TRACE").is_some(),
            rip0: std::env::var_os("LITEBOX_DIAG_WAIT4GATE").is_some(),
            fataldump: std::env::var_os("LITEBOX_DIAG_FATALDUMP").is_some(),
            fault_vq: std::env::var_os("LITEBOX_DIAG_FAULT_VQ").is_some(),
            fault_module: std::env::var_os("LITEBOX_DIAG_FAULT_MODULE").is_some(),
            alloc_vec: std::env::var_os("LITEBOX_DIAG_ALLOC_VEC").is_some(),
            avfull: std::env::var_os("LITEBOX_DIAG_AVFULL").is_some(),
            allow_wer: std::env::var_os("LITEBOX_DIAG_ALLOW_WER").is_some(),
            interrupt: std::env::var_os("LITEBOX_DIAG_INTERRUPT").is_some(),
            codewatch_selftest: codewatch.as_deref().is_some_and(|v| v == "selftest"),
            codewatch: codewatch.is_some(),
            watchaddr: std::env::var("LITEBOX_DIAG_WATCHADDR")
                .ok()
                .and_then(|s| usize::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                .filter(|&a| a != 0),
            forkverify_off: std::env::var_os("LITEBOX_FORKVERIFY_OFF").is_some(),
        }
    }
}

static VEH_GATES: OnceLock<VehGates> = OnceLock::new();

/// The process-wide [`VehGates`], initialising them on first use.
///
/// [`WindowsUserland::new`] calls this before any guest thread exists, so every later call --
/// including every one from inside the exception handler -- is a plain read of initialised memory.
pub(crate) fn veh_gates() -> &'static VehGates {
    VEH_GATES.get_or_init(VehGates::read_environment)
}

/// Whether `LITEBOX_VEH_TRACE` tracing is enabled. See [`VehGates`].
pub(crate) fn veh_trace_enabled() -> bool {
    veh_gates().veh_trace
}

/// Whether the targeted `rip == 0` crash diagnostics (`LITEBOX_DIAG_WAIT4GATE=1`) are enabled.
///
/// Deliberately a separate, much narrower gate than [`veh_trace_enabled`]: full `LITEBOX_VEH_TRACE`
/// emits a per-instruction trace that perturbs timing enough to hide the crash being investigated,
/// whereas this gate only enables a handful of one-off prints around fork-child verification and
/// the fault itself.
pub(crate) fn diag_rip0_enabled() -> bool {
    veh_gates().rip0
}

/// Whether the fatal-fault-only dump (`LITEBOX_DIAG_FATALDUMP=1`) is enabled.
///
/// Pass-39/40: split out from [`veh_trace_enabled`] because that gate also turns on
/// `fork_verify`'s own per-single-step trace print on every trapped instruction of a
/// verifying fork child (thousands of prints per fork, each an expensive `eprintln!` to a
/// piped terminal) -- perturbing timing enough in practice to hide the very crash under
/// investigation (see FINDINGS.txt pass 39 item 3). This gate covers only the rare block
/// that dumps rip bytes/regs on an actual fatal fault (rip==0 privileged-instruction
/// class, or a near-null access violation), so it is cheap enough to leave on for an entire
/// local repro run without masking the bug.
pub(crate) fn diag_fataldump_enabled() -> bool {
    let gates = veh_gates();
    gates.veh_trace || gates.fataldump
}

/// Minimal-footprint entry point registered with `AddVectoredExceptionHandler`, in place of
/// [`vectored_exception_handler`] directly.
///
/// # Why this exists
///
/// Guest code runs with the real CPU `rsp`/`rbp` holding a GUEST-address value (this platform
/// has no separate emulated guest stack -- see `switch_to_guest`'s doc comment) -- memory that,
/// while genuinely backed by real committed pages, has no relationship to Windows' own per-thread
/// stack-overflow/guard-page bookkeeping (`TEB.StackBase`/`StackLimit`). Windows always invokes a
/// registered VEH callback using whatever `rsp` the CPU held at fault time -- there is no
/// mechanism to make it invoke the callback on a different, pre-chosen stack. A `fork()` child's
/// very first exception (routinely the "Windows cleared FS_BASE upon scheduling" condition the
/// full [`vectored_exception_handler`] below already repairs, just far down in its body) was
/// confirmed live to make that full handler's own sizeable stack frame (several hundred lines of
/// body, multiple large diagnostic branches) trip `__chkstk`'s guard-page probe against that
/// unrecognized guest-address memory, which recursively re-faulted at the same instruction on
/// every retry -- exhausting the thread's REAL stack one guard page at a time until the process
/// died with a genuine stack overflow, never once reaching the repair the full handler already
/// has.
///
/// This entry point's own frame is kept deliberately minimal (raw register/memory reads only, no
/// heap allocation) specifically so it can safely run in that same narrow, guest-address-stack
/// window. Two things happen here, in order:
///
/// 1. A fast path for exactly the one condition the guest hits almost immediately after a
///    `fork()` resume (`EXCEPTION_ACCESS_VIOLATION`, `rdfsbase() == 0`, a genuine instruction
///    rather than a null `Rip`): repaired in place with a handful of instructions, matching
///    exactly what [`vectored_exception_handler`]'s own later, redundant repair already does, and
///    returns immediately -- avoiding entering the full handler's larger frame at all for the
///    single most common case.
/// 2. For every other exception code/condition, this thread's own `TlsState.host_sp`/`host_bp`
///    (populated by `run_thread_arch`'s prologue before guest code ever runs, and always a real,
///    Windows-registered stack address for this exact thread -- see those fields' own doc
///    comments) are swapped into the LIVE `rsp`/`rbp` registers before `call`ing the full
///    [`vectored_exception_handler`], so its own much larger frame -- and everything it in turn
///    calls (`fork_verify::on_single_step`'s instruction-decode logic, the diagnostic branches,
///    ...) -- runs on real, guard-page-protected memory instead of the guest's. The original
///    (guest-address) `rsp`/`rbp` are saved first and restored after the call returns, immediately
///    before this function's own `ret` -- the full handler communicates any guest-context changes
///    (including a deliberate `Rsp`/`Rbp` rewrite, e.g. `exception_callback`'s own redirect) via
///    the `CONTEXT` structure in memory, which this stack swap never touches, so nothing about the
///    full handler's existing behavior changes -- only which stack ITS OWN Rust code executes on
///    while deciding what to write into that `CONTEXT`.
#[unsafe(naked)]
unsafe extern "system" fn vectored_exception_handler_entry(
    exception_info: *mut EXCEPTION_POINTERS,
) -> i32 {
    core::arch::naked_asm!(
        "
        // UNWIND INFO (`.seh_proc`/`.seh_endproc` with an empty prologue): without these, this
        // naked function emits NO `.pdata`/`.xdata` entry at all, and Windows' own stack
        // unwinder (`RtlVirtualUnwind` -> `RtlpxVirtualUnwind` -> `RtlpUnwindPrologue`) treats
        // any frame it finds here as a LEAF function -- i.e. it assumes the return address sits
        // at `[rsp]` and that no non-volatile register was saved. Neither assumption holds once
        // `.Lswap` below has switched `rsp` to this thread's separate host stack, so the
        // unwinder reads a return address and a frame chain out of unrelated stack bytes and
        // then keeps walking from that garbage. Confirmed live (this pass) via `cdb .fnent`
        // against the release binary: this function reported `No function entry`, while its
        // immediate neighbour `run_thread_arch` (which DOES carry `.seh_proc run_thread`)
        // reported a correct entry -- and the captured fatal fault in a real XFCE repro was
        // exactly `ntdll!RtlpUnwindPrologue+0x11a` (`mov rcx, qword ptr [r8]`) with `r8 = 0x2`,
        // reached from `ntdll!RtlpxVirtualUnwind+0x109`, i.e. the unwinder itself dereferencing
        // a garbage unwind-info pointer it derived from this missing entry. The characteristic
        // split-word register damage seen in that investigation (`rsp=0x2f00000030`,
        // `rsi=0xffffffff00000000`, `rdi=0x401000001` -- plausible low halves, garbage high
        // halves) is the same unwinder restoring non-volatile registers from misidentified
        // stack slots, NOT a truncation bug in this crate's own code.
        //
        // An EMPTY prologue (`.seh_endprologue` immediately after `.seh_proc`, no
        // `.seh_pushreg`/`.seh_stackalloc`/`.seh_setframe`) is the ACCURATE description here,
        // not a placeholder: this function never pushes a non-volatile register and never
        // establishes a frame pointer on the CALLER stack. Everything it saves (the caller's
        // `rsp`/`rbp`/`r8`) is stored into the per-depth scratch slot on the SEPARATE host
        // stack it switches to, and every one of those is restored before the `ret` below, so
        // at both entry and exit the caller-visible state is exactly a frameless function's.
        // Declaring that truthfully is what lets the unwinder skip straight to the real return
        // address at `[rsp]` on entry instead of inventing one.
        .seh_proc vectored_exception_handler_entry
        .seh_endprologue

        // rcx = exception_info (EXCEPTION_POINTERS*), per the x64 'extern system' ABI. No stack
        // use yet, so no shadow space/alignment to establish for this first, read-only portion.
        mov     rax, [rcx]           // rax = ExceptionRecord*
        mov     edx, [rax]           // edx = ExceptionRecord->ExceptionCode (i32)

        // WHITELIST, not a blacklist: decline every exception code the full handler does not
        // actually act on, before touching anything else.
        //
        // `vectored_exception_handler` triages exactly four codes -- `EXCEPTION_ACCESS_VIOLATION`
        // (guest page faults, the FS_BASE-cleared repair, the CoW and exception-table paths),
        // `EXCEPTION_SINGLE_STEP` (`fork_verify`'s instruction walk), and the two that a guest's
        // own `syscall`/privileged instruction can raise, `EXCEPTION_ILLEGAL_INSTRUCTION` and
        // `0xC0000096` (`STATUS_PRIVILEGED_INSTRUCTION`). Everything else fell through to
        // `.Lswap`, which swaps stacks and calls that handler anyway, purely to have it decide it
        // has nothing to do.
        //
        // That is not merely wasted work, it is actively harmful for two codes in particular:
        //
        //   - `EXCEPTION_STACK_OVERFLOW`. Windows delivers it once, on the guard page, and the
        //     thread has only the remaining guard region to act in. The Rust runtime's own
        //     handler uses exactly that window to name the overflowing thread and abort cleanly;
        //     spending it on a stack swap and a large Rust frame instead loses the message, and
        //     the process dies as a bare `0xC0000005` saying nothing. Observed directly against
        //     `litebox_shim_linux`'s own `stdio::tests::test_stdio_flags_with_dup`.
        //   - `0xE06D7363` (`STATUS_MSVC_CPP_EXCEPTION`), which is what a Rust `panic!` raises on
        //     an MSVC target. Every panic in the process -- including an ordinary assertion
        //     failure in a unit test -- was entering this trampoline and being carried through a
        //     stack swap on its way to a handler with no interest in it.
        //
        // Declining these is also what makes registering FIRST in the VEH chain safe (see
        // `AddVectoredExceptionHandler`'s call site): first place in the chain is only correct
        // for a handler that looks at exactly what is its own.
        cmp     edx, {EXCEPTION_ACCESS_VIOLATION}
        je      .Lours
        cmp     edx, {EXCEPTION_SINGLE_STEP}
        je      .Lours
        cmp     edx, {EXCEPTION_ILLEGAL_INSTRUCTION}
        je      .Lours
        cmp     edx, {EXCEPTION_PRIV_INSTRUCTION}
        je      .Lours
        // Not one of ours. No TLS read, no stack swap, no Rust.
        mov     eax, {EXCEPTION_CONTINUE_SEARCH}
        ret

    .Lours:
        // Read this thread's TlsState pointer via the same TEB-slot lookup pattern
        // `syscall_callback` already uses elsewhere in this file. Needed by both the fast path
        // below and the host-stack swap before falling through to the full handler, so done once,
        // unconditionally, up front.
        mov     r8d, DWORD PTR [rip + {TLS_INDEX}]
        mov     r8, QWORD PTR gs:[r8 * 8 + {TEB_TLS_SLOTS_OFFSET}]
        test    r8, r8
        je      .Lsearch             // TLS not installed on this thread -- matches the full handler's
                                      // own `get_tls_ptr()`-is-`None` early return: not our exception.

        cmp     edx, {EXCEPTION_ACCESS_VIOLATION}
        jne     .Lswap

        rdfsbase r9
        test    r9, r9
        jne     .Lswap               // FS base is not zero; not this condition

        mov     r10, [rcx + 8]       // r10 = ContextRecord*
        mov     r11, [r10 + {CONTEXT_RIP}]
        test    r11, r11
        je      .Lswap               // Rip == 0: not a real instruction, let the full handler triage it

        mov     rax, QWORD PTR [r8 + {GUEST_FS_BASE}]
        test    rax, rax
        je      .Lswap               // no saved FS base to restore; let the full handler decide

        wrfsbase rax
        mov     eax, {EXCEPTION_CONTINUE_EXECUTION}
        ret

    .Lswap:
        // TLS is installed (r8 non-null, checked above) but `host_sp` is only populated by
        // `run_thread_arch`'s own prologue, which runs strictly after `install_tls` -- there is a
        // narrow window on every thread between the two where `TlsState` exists but `host_sp` is
        // still its `TlsState::new()` default of null. Guard against swapping onto a null stack in
        // that window; fall through to calling the full handler on whatever stack is already live,
        // exactly as this trampoline did not exist -- strictly no worse than the pre-existing
        // behavior for a window this narrow.
        mov     r11, QWORD PTR [r8 + {HOST_SP}]
        test    r11, r11
        je      .Lcall_here_startup

        // Save the live (guest-address) rsp/rbp, then swap to this thread's real, Windows-
        // registered host stack (`TlsState.host_sp`/`host_bp`) before calling the full handler, so
        // ITS much larger frame -- and everything it calls -- runs on real, guard-page-protected
        // memory instead of guest-address memory `__chkstk` cannot safely probe.
        //
        // REENTRANCY: this trampoline can fire again before a prior, still-live invocation on
        // this SAME thread returns (confirmed live: a `LITEBOX_PROCESS_FORK=1` repro hit over
        // 3000 nested reentries here before segfaulting -- see `TlsState::veh_depth`'s doc
        // comment). A single FIXED `host_sp - 64` scratch slot is therefore unsafe: a nested
        // invocation would overwrite the outer invocation's still-live saved guest rsp/rbp (and
        // saved `r8`) right out from under it. Use `veh_depth` (`Cell<u32>`, already needed for
        // the reentrancy diagnostic) to give each nesting level its own 64-byte slot instead:
        // `host_sp - 64 - depth*64`, capped at `VEH_DEPTH_CAP` slots so a genuinely runaway
        // cascade cannot walk this scratch region into the separately-reserved exception-record
        // slots further down (see `VEH_DEPTH_CAP`'s own doc comment for the exact non-overlapping
        // layout). Depth 0 (the overwhelming common, non-reentrant case) keeps today's exact
        // `host_sp - 64` slot unchanged.
        mov     r9d, DWORD PTR [r8 + {VEH_DEPTH}]
        cmp     r9d, {VEH_DEPTH_CAP}
        jae     .Lsearch             // past the cap: a fault recurring this deep on this thread
                                      // cannot be safely handled at all -- see .Lcall_here_startup's
                                      // doc comment for why this must bail out via
                                      // EXCEPTION_CONTINUE_SEARCH rather than keep calling the
                                      // full handler unbounded on a stack that is not being swapped.
        inc     DWORD PTR [r8 + {VEH_DEPTH}]
        imul    r9d, r9d, {VEH_FRAME_STRIDE}
        mov     r9d, r9d             // zero-extend the 32-bit product into a usable 64-bit index

        mov     r10, rsp
        mov     rax, rbp
        mov     rbp, QWORD PTR [r8 + {HOST_BP}]
        // Reserve `VEH_FRAME_STRIDE` bytes at this depth's own slot (a multiple of 16, preserving
        // the Win64 ABI's required alignment at the `call` below): 32 for the shadow space the
        // callee is entitled to write into, 16 to save the original guest rsp/rbp across the
        // call, 16 more to save `r8` (this thread's `TlsState*`) itself, and -- critically -- the
        // remainder as the actual STACK `vectored_exception_handler` and everything it calls will
        // consume below this point.
        //
        // This stride was previously 64, which covered only the three saved values above and gave
        // the callee NO reserved stack of its own. Since the callee's frame grows down from
        // `rsp`, at `veh_depth == 1` the nested invocation's slot (`host_sp - 128`) sat only 64
        // bytes below the outer invocation's (`host_sp - 64`) while the outer's live frame was
        // kilobytes deep -- so the nested handler's locals overwrote the still-live outer
        // handler's own frame, `exception_record` copy and fault ring included. Confirmed live:
        // a `touch` repro crashed with `veh_depth=0x1` and the outer invocation then read an
        // `ExceptionCode` of `0x470041`, which is not a Windows status code at all (severity bits
        // 00 = success, and its bytes are UTF-16LE for the two characters A and G, i.e. raw string
        // data from the nested frame), after which dispatch followed a garbage `rip` (`0x22`,
        // `0x40`) into the
        // wild-jump cascade ending at `[diag-unrecov-av-giveup]`. Giving each nesting level a
        // real, frame-sized slice is what makes the per-depth scheme actually mean what its name
        // says.
        //
        // `r8` is a
        // CALLER-SAVED/volatile register per the Win64 ABI, so `vectored_exception_handler` (an
        // ordinary Rust function, not obligated to preserve it) is free to clobber it, and this
        // trampoline still needs a valid `r8` AFTER the call to `dec` the depth counter back
        // down. Confirmed live: before this fix, `r8` was assumed to survive the call unchanged,
        // and the resulting `dec DWORD PTR [r8 + VEH_DEPTH]` on a clobbered `r8` was ITSELF the
        // faulting instruction in a real, reproduced crash (disassembly confirmed the fault `rip`
        // landed exactly on that `dec`) -- not a pre-existing bug, but one this depth-tracking
        // fix introduced and is corrected here in the same pass.
        sub     r11, r9
        lea     rsp, [r11 - {VEH_FRAME_STRIDE}]
        mov     QWORD PTR [rsp + 32], r10
        mov     QWORD PTR [rsp + 40], rax
        mov     QWORD PTR [rsp + 48], r8

        call    {vectored_exception_handler}

        // CRITICAL: eax/rax at this point holds the real return value from
        // vectored_exception_handler and must reach this trampoline's own ret unmodified.
        // Restore rsp/rbp/r8 via r9/r10 instead of reusing rax as scratch -- an earlier version
        // of this trampoline clobbered rax with the saved rbp value here, silently discarding
        // the real return value on every call.
        mov     r8,  QWORD PTR [rsp + 48]
        mov     r10, QWORD PTR [rsp + 32]
        mov     r9,  QWORD PTR [rsp + 40]
        mov     rsp, r10
        mov     rbp, r9
        dec     DWORD PTR [r8 + {VEH_DEPTH}]
        ret

    .Lcall_here_startup:
        // Only the narrow `host_sp == 0` early-startup window lands here now (the
        // `VEH_DEPTH_CAP` case now bails to `.Lsearch` directly, above -- see that jump's own
        // comment: AGENTS.md continuation, this pass, root-caused via the first-ever WER
        // minidump this investigation captured). This window is rare and inherently
        // non-recursive (it exists only before `run_thread_arch`'s prologue populates
        // `host_sp`), so falling through to call the full handler on whatever stack is already
        // live remains safe here, unchanged from the original behavior.
        jmp     {vectored_exception_handler}

    .Lsearch:
        // DIAG (this investigation pass): this is the ONE exit in the whole fault path that
        // previously logged absolutely nothing -- the trampoline returns
        // EXCEPTION_CONTINUE_SEARCH without ever entering Rust, so the exception table is never
        // consulted and no diagnostic in `vectored_exception_handler` can possibly observe it.
        // It is reached both when `veh_depth` is at `VEH_DEPTH_CAP` and from the earlier
        // non-AV/no-TLS guards above. `lock inc` on a plain static is the only instrumentation
        // safe here: no stack, no registers clobbered (flags are dead on this path -- the very
        // next instruction loads `eax` with a constant and returns), no call, no allocation.
        // Rust-side diagnostics read this counter to report how many faults took this invisible
        // exit.
        lock inc QWORD PTR [rip + {LSEARCH_COUNT}]
        mov     eax, {EXCEPTION_CONTINUE_SEARCH}
        ret
        .seh_endproc
        ",
        LSEARCH_COUNT = sym LSEARCH_EXIT_COUNT,
        EXCEPTION_ACCESS_VIOLATION = const Win32_Foundation::EXCEPTION_ACCESS_VIOLATION,
        EXCEPTION_SINGLE_STEP = const Win32_Foundation::EXCEPTION_SINGLE_STEP,
        EXCEPTION_ILLEGAL_INSTRUCTION = const Win32_Foundation::EXCEPTION_ILLEGAL_INSTRUCTION,
        EXCEPTION_PRIV_INSTRUCTION = const 0xC000_0096_u32.cast_signed(),
        EXCEPTION_CONTINUE_EXECUTION = const EXCEPTION_CONTINUE_EXECUTION,
        EXCEPTION_CONTINUE_SEARCH = const EXCEPTION_CONTINUE_SEARCH,
        CONTEXT_RIP = const core::mem::offset_of!(
            windows_sys::Win32::System::Diagnostics::Debug::CONTEXT,
            Rip
        ),
        TLS_INDEX = sym TLS_INDEX,
        TEB_TLS_SLOTS_OFFSET = const 5248,
        GUEST_FS_BASE = const core::mem::offset_of!(TlsState, guest_fs_base),
        HOST_SP = const core::mem::offset_of!(TlsState, host_sp),
        HOST_BP = const core::mem::offset_of!(TlsState, host_bp),
        VEH_DEPTH = const core::mem::offset_of!(TlsState, veh_depth),
        VEH_DEPTH_CAP = const VEH_DEPTH_CAP,
        VEH_FRAME_STRIDE = const VEH_FRAME_STRIDE,
        vectored_exception_handler = sym vectored_exception_handler,
    );
}

/// Whether the instruction at `rip` genuinely has an `FS:` segment-override prefix (opcode byte
/// `0x64`), i.e. is a real `%fs:`-relative access that a stale/zeroed `FS_BASE` could plausibly
/// explain.
///
/// The FS_BASE-reset repair sites below (both the guest-mode and host-mode occurrences) used to
/// treat *every* `EXCEPTION_ACCESS_VIOLATION` seen while `rdfsbase() == 0` as "Windows cleared
/// this thread's FS_BASE MSR again, just restore it and retry" -- with no check that the faulting
/// instruction actually reads/writes through the FS segment at all. Since Windows clearing
/// `FS_BASE` is asynchronous ("apparently as part of ordinary scheduling", per the repair sites'
/// own comments) it can coincide with an *unrelated* real fault -- e.g. a genuine null-pointer
/// dereference in guest code -- at which point `wrfsbase`-then-retry does nothing to fix the real
/// problem and the identical instruction re-faults immediately, forever: a silent, non-converging
/// repair loop with the exact same observable shape (`EXCEPTION_ACCESS_VIOLATION`, `rdfsbase() ==
/// 0`, same `rip` every time) as the real FS_BASE-reset case, confirmed live via
/// `LITEBOX_VEH_TRACE=1` against a `process.title = <string>` repro under Node.js (a plain `mov
/// rdx, [rdx+0x788]` with `rdx` already null -- no FS override, no REX.W-only coincidence -- was
/// being "repaired" and retried unboundedly). This mirrors the file's own precedent for exactly
/// this class of bug: the `Rip != 0` guard a few lines below this function's call sites was added
/// after an earlier livelock (1809+ repeated repairs, no forward progress) was found to be the
/// same shape -- blindly retrying a fault the repair could never actually fix.
///
/// x86_64 legacy prefixes (`LOCK` `0xF0`, `REP`/`REPNE` `0xF2`/`0xF3`, the six segment overrides
/// `0x2E`/`0x36`/`0x3E`/`0x26`/`0x64`/`0x65`, operand-size `0x66`, address-size `0x67`) may appear
/// in any order before the opcode, followed optionally by a single REX prefix (`0x40`-`0x4F`)
/// immediately before the opcode itself -- per the Intel SDM, at most four legacy prefixes are
/// architecturally meaningful, though nothing stops more from being *present* in an
/// (unusual/malformed) encoding. Scanning a bounded window (the SDM's own 15-byte maximum
/// instruction length) and stopping at the first REX/opcode-shaped byte once at least one
/// definite non-prefix byte class is hit would require a much more complete decoder than this
/// narrow check needs; instead, scan only the legacy-prefix run (bounded to 4 bytes, matching the
/// SDM's own limit) and check whether `0x64` appears anywhere in it -- false-negatives (missing a
/// real FS-relative access) are safe here, since they only fall through to the normal exception
/// path instead of repairing, at worst turning a real FS_BASE-reset case into a diagnosable crash
/// rather than a silent hang; false-positives (wrongly treating a non-FS-relative fault as
/// FS-relative) are what this function exists to eliminate, and requiring the exact `0x64` byte to
/// be genuinely present in the prefix run has none.
/// Read one `usize` from `addr`, but ONLY after confirming the page is committed and readable.
///
/// The unrecoverable-AV diagnostics walk memory pointed at by registers captured at a fault whose
/// whole nature is that those registers may be garbage. Confirmed live: `context.Rsp` at one such
/// fault was `0xc0000008` -- not a misaligned stack address but an NTSTATUS-shaped value
/// (`STATUS_INVALID_HANDLE`) sitting where a pointer should be. Dereferencing that raises a
/// SECOND access violation from inside the VEH itself, which silently kills the dump: the handler
/// produces no further output and the evidence it exists to capture is lost.
///
/// So validity is checked BEFORE the read rather than recovered afterwards -- a fault inside a
/// fault handler has no good recovery path. `VirtualQuery` is the same primitive the neighbouring
/// `diag-unrecov-av-pagestate` block already uses. Returns `None` when the address is not
/// committed or not readable, so the caller can print why it stopped instead of dying.
fn diag_probe_read_usize(addr: usize) -> Option<usize> {
    // A null address cannot be mapped; skip the syscall entirely.
    if addr == 0 {
        return None;
    }
    let mut mbi = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
    let queried = unsafe {
        Win32_Memory::VirtualQuery(
            addr as *const c_void,
            &mut mbi,
            core::mem::size_of::<Win32_Memory::MEMORY_BASIC_INFORMATION>(),
        )
    };
    if queried == 0 || mbi.State != Win32_Memory::MEM_COMMIT {
        return None;
    }
    // PAGE_NOACCESS/PAGE_GUARD would fault on read just as surely as an unmapped page does.
    const READABLE: u32 = Win32_Memory::PAGE_READONLY
        | Win32_Memory::PAGE_READWRITE
        | Win32_Memory::PAGE_WRITECOPY
        | Win32_Memory::PAGE_EXECUTE_READ
        | Win32_Memory::PAGE_EXECUTE_READWRITE
        | Win32_Memory::PAGE_EXECUTE_WRITECOPY;
    if mbi.Protect & READABLE == 0 || mbi.Protect & Win32_Memory::PAGE_GUARD != 0 {
        return None;
    }
    // The 8 bytes must not straddle out of the committed region.
    let region_end = (mbi.BaseAddress as usize).saturating_add(mbi.RegionSize);
    if addr.saturating_add(8) > region_end {
        return None;
    }
    // SAFETY: the page is committed and readable per the query above. `read_unaligned` because a
    // register captured mid-fault carries no alignment guarantee.
    Some(unsafe { (addr as *const usize).read_unaligned() })
}

fn faulting_instruction_has_fs_override(rip: usize) -> bool {
    let mut buf = [0u8; 4];
    let n = fork_verify::read_code_bytes_for_diagnostics(rip, &mut buf);
    buf[..n].contains(&0x64)
}

/// PRD `veh-frame-stride-has-no-overflow-guard`: `VEH_FRAME_STRIDE` (see that constant's own doc
/// comment) has been silently too small twice before -- 64 bytes, then 4096 bytes -- and both
/// times the symptom was cross-frame stack corruption found only by a multi-session live crash
/// hunt, because nothing in the tree actually detects a too-small stride: the const assertions
/// near `exception_record_ptr` only prove the per-depth frames and the exception-record slots
/// don't collide with EACH OTHER, never that any one frame's actual peak stack usage fits inside
/// its own `VEH_FRAME_STRIDE`-sized slice.
///
/// Stamps a fixed canary value 64 bytes above the FLOOR of the *next* nesting level's own slice --
/// the same 64 bytes that level's own trampoline entry unconditionally reserves for its shadow
/// space and its three saved registers, see `vectored_exception_handler_entry`'s `.Lswap` comment
/// -- at construction, then re-reads it on `Drop`, i.e. after every possible return path out of
/// `vectored_exception_handler` (this function has over a dozen distinct `return` sites, so a
/// single check at the bottom would miss most of them). A live overflow -- this invocation's own
/// stack usage, or anything it calls transitively (`fork_verify::on_single_step`, the AV-path
/// stale-pointer healers, `eprintln!` formatting, iced-x86 decoding) -- reaching past this
/// invocation's `VEH_FRAME_STRIDE` budget corrupts the canary irreversibly: the write already
/// happened, so popping the stack pointer back up on return does not undo it, which is exactly why
/// checking at `Drop` time (long after the deepest actual stack depth was reached) still reliably
/// catches it.
///
/// Deliberately does NOT write the canary from the naked-asm trampoline itself, where the
/// historical bug lived: that address sits up to one whole `VEH_FRAME_STRIDE` (8 KiB, two pages)
/// below the trampoline's own `rsp` at that point, and a single write that far past the last
/// touched page risks the exact guard-page-skip hazard `exception_record_ptr`'s own write already
/// had to solve via an explicit `VirtualAlloc(MEM_COMMIT)` (see that call site's doc comment) --
/// reusing that same proven technique here, from ordinary (non-naked) Rust code that already has a
/// ordinary stack frame of its own, is safer than hand-rolling the same fix a second time in
/// fragile trampoline asm on the fault-recovery path itself.
struct VehFrameCanaryGuard {
    addr: *mut u64,
}

/// Arbitrary but recognizable in a hex dump: "VEHCANAR" read as big-endian ASCII bytes.
const VEH_FRAME_CANARY: u64 = 0x5645_4843_414e_4152;

impl VehFrameCanaryGuard {
    /// `host_sp`/`depth` must be this invocation's own `TlsState::host_sp`/post-increment
    /// `TlsState::veh_depth` (i.e. exactly what `vectored_exception_handler_entry`'s trampoline
    /// just used to place this invocation's own slice) -- this invocation's own floor is
    /// `host_sp - depth * VEH_FRAME_STRIDE`, so the next level's floor, and the canary 64 bytes
    /// above it, is one more `VEH_FRAME_STRIDE` further down. Returns `None` for `depth == 0`
    /// (should never happen on the depth-tracked swap path, but stays defensive rather than
    /// computing a nonsense address on an unexpected value) so callers can skip the guard entirely
    /// on the narrow `.Lcall_here_startup` fallback, which never went through the depth-tracked
    /// slice scheme in the first place.
    fn new(host_sp: *mut u128, depth: u32) -> Option<Self> {
        if depth == 0 {
            return None;
        }
        let next_floor = (host_sp as usize)
            .wrapping_sub((depth as usize + 1).wrapping_mul(VEH_FRAME_STRIDE as usize));
        let addr = next_floor.wrapping_add(64) as *mut u64;
        // SAFETY: mirrors `exception_record_ptr`'s own explicit-commit write exactly (same file,
        // same reasoning) -- commits precisely the one page this write needs before touching it,
        // rather than relying on a single far jump to trigger ordinary sequential guard-page
        // growth.
        unsafe {
            let commit_page = (addr as *mut u8).map_addr(|a| a & !0xFFF);
            let _ = Win32_Memory::VirtualAlloc(
                commit_page.cast(),
                4096,
                Win32_Memory::MEM_COMMIT,
                Win32_Memory::PAGE_READWRITE,
            );
            addr.write_volatile(VEH_FRAME_CANARY);
        }
        Some(Self { addr })
    }
}

impl Drop for VehFrameCanaryGuard {
    fn drop(&mut self) {
        // SAFETY: `addr` was committed and written by `new` above; `Drop` runs on the same thread,
        // strictly after that write, on every exit path.
        let value = unsafe { self.addr.read_volatile() };
        if value != VEH_FRAME_CANARY {
            diag_raw_print(
                b"[diag-veh-frame-stride-overflow] addr=0x",
                self.addr as usize,
                b" corrupted_value=0x",
                value as usize,
            );
            // Same reasoning as the unrecovered-AV circuit breaker just below in this file: a
            // stack slice known to have been overrun risks silent corruption of a neighboring
            // nesting level's still-live frame -- exactly the historical VEH_FRAME_STRIDE bug this
            // guard exists to catch. Fail fast rather than let the trampoline unwind back into a
            // possibly-corrupted outer frame. `RaiseFailFastException` accepts null record/context
            // pointers per its own contract; there is no single exception record that describes
            // "a canary write detected stack corruption", unlike the real AV/AV-recovery call
            // sites elsewhere in this file that pass the genuine faulting record.
            unsafe {
                windows_sys::Win32::System::Diagnostics::Debug::RaiseFailFastException(
                    core::ptr::null(),
                    core::ptr::null(),
                    0,
                );
            }
        }
    }
}

unsafe extern "system" fn vectored_exception_handler(
    exception_info: *mut EXCEPTION_POINTERS,
) -> i32 {
    // AGENTS.md pass 266: `RaiseFailFastException` (the LITEBOX_DIAG_ALLOW_WER escape hatch,
    // pass 246) raises `STATUS_STACK_BUFFER_OVERRUN` (0xC0000409) specifically because real
    // Windows treats it as non-continuable and non-interceptable by ordinary SEH/VEH handlers --
    // it is meant to go straight to Windows' own crash-reporting (WER) path. Live captures
    // (pass 248, confirmed again this pass) showed this VEH intercepting it anyway and recursing
    // back into itself instead of ever reaching WER. Bail out immediately, before any other
    // processing in this function, for exactly this one exception code -- restoring the
    // "non-interceptable" semantics real Windows code relies on and letting the fail-fast path
    // actually reach WER as intended.
    if unsafe { (*(*exception_info).ExceptionRecord).ExceptionCode } == 0xC000_0409_u32.cast_signed()
    {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    // See `WindowsUserland::init_thread_gs_base`'s doc comment: the same "Windows clears a
    // non-standard segment-base MSR under scheduling pressure" behavior already known and
    // repaired for `FS_BASE` plausibly affects `GS_BASE` too, and `GS_BASE` backs Windows' OWN
    // TEB access (`ntdll`, `TlsGetValue` below, this thread's exception dispatch machinery
    // itself). A no-op when `GS_BASE` already reads back correctly; cheap enough to check
    // unconditionally, this early, before anything in this handler (including `get_tls_ptr`'s own
    // `TlsGetValue` call, which depends on a working TEB) risks running with it wrong.
    WindowsUserland::restore_thread_gs_base_if_cleared();

    // Overflow guard for this invocation's own `VEH_FRAME_STRIDE` slice (PRD
    // `veh-frame-stride-has-no-overflow-guard`, see `VehFrameCanaryGuard`'s own doc comment for
    // the full reasoning). Placed as early as practical -- right after the GS_BASE repair, which
    // must run first since it backs this function's own TLS access -- so the guard's lifetime
    // covers essentially the whole function body, including every deep call this handler makes.
    // `None` on the narrow no-TLS/no-swap fallback paths, which have no dedicated slice to guard.
    let _veh_frame_canary_guard = get_tls_ptr().and_then(|p| {
        let tls = unsafe { &*p };
        VehFrameCanaryGuard::new(tls.host_sp.get(), tls.veh_depth.get())
    });

    // DIAG (LITEBOX_DIAG_ALLOC_VEC=1 investigation continuation): unconditional (no gate other
    // than a call-count cap), allocation-free entry counter -- answers "is VEH even being
    // entered again after the first single-step" directly, independent of every other diagnostic
    // gate in this function (all of which run later and could themselves be skipped for reasons
    // unrelated to whether VEH fired at all).
    static VEH_ENTRY_COUNT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    if veh_gates().alloc_vec {
        let n = VEH_ENTRY_COUNT.fetch_add(1, Ordering::Relaxed);
        if n < 10 {
            let code = unsafe { (*(*exception_info).ExceptionRecord).ExceptionCode };
            diag_raw_print(b"[diag_veh_entry] n=0x", n, b" code=0x", code as usize);
        }
    }

    // Always-on, allocation-free per-thread ring of the last few (code, rip, is_in_guest) fault
    // triples seen by this handler on THIS thread -- added specifically because a secondary fault
    // inside `ntdll!RtlpUnwindPrologue` (confirmed live, this investigation: a fault whose
    // unwind-time crash is a downstream symptom, not the true root cause) means the diagnostics at
    // the bottom of this function only ever see the LAST fault, by which point the actually
    // informative FIRST fault that triggered Windows' own unwind has already been dispatched and
    // is otherwise unrecoverable. `std::cell::Cell`-based thread_local, no heap allocation, no
    // locking -- safe to read/write even from deep inside exception handling.
    std::thread_local! {
        // `Option<bool>` (not `bool`): see the `is_in_guest_state` comment below -- `None` means
        // "could not determine" and must stay distinguishable from a confirmed `Some(false)`.
        static RECENT_FAULTS: RefCell<[(i32, u64, u64, Option<bool>); 4]> =
            const { RefCell::new([(0, 0, 0, None); 4]) };
        // Ring of (faulting_rip, recover_fixup_addr) pairs for every exception-table recovery
        // this thread has taken -- see the `context.Rip = recover` call site's own doc comment.
        static RECOVERY_LOG: RefCell<[(u64, u64); 4]> = const { RefCell::new([(0, 0); 4]) };
    }
    {
        let code = unsafe { (*(*exception_info).ExceptionRecord).ExceptionCode };
        // ATOMICITY FIX: `rip` and `rsp` used to come from two SEPARATE dereferences of the live,
        // OS-owned `CONTEXT` (`(*(*exception_info).ContextRecord).Rip` then `.Rsp`). This function
        // runs before the `context_snapshot` copy a few hundred lines below is taken, and nothing
        // stops `ThreadHandle::interrupt`'s `SuspendThread`/`SetThreadContext` or
        // `ctxwatch_arm_other_threads`' debug-register rewrites on another thread from mutating
        // that same CONTEXT between the two reads -- exactly the torn-read class already fixed
        // for the later diagnostics by commit 9b124ed, which predates this early ring capture and
        // never covered it. Take one struct copy up front (same justification as
        // `context_snapshot`: `CONTEXT` is `Copy`, a fixed-size stack copy, no allocation, no
        // call that can itself fault) and read both fields from that single snapshot.
        let ctx_snapshot = unsafe { *(*exception_info).ContextRecord };
        let rip = ctx_snapshot.Rip;
        let rsp = ctx_snapshot.Rsp;
        // TRI-STATE FIX: `get_tls_ptr()` returning `None` means this thread has no TLS slot at
        // all, i.e. "could not determine whether this is a guest thread" -- not "confirmed not a
        // guest thread". Collapsing that via `.unwrap_or(false)` made the two cases print
        // identically (`is_in_guest=false`) everywhere this value is logged, so a reader could
        // never tell "genuinely a host thread" apart from "thread identification failed here".
        // Keep the tri-state for the ring/log; `this_is_in_guest` below stays the existing
        // conservative bool (unknown treated as not-guest) for this function's own gating logic,
        // which is unaffected by this fix.
        let is_in_guest_state: Option<bool> =
            get_tls_ptr().map(|p| unsafe { (*p).is_in_guest.get() });
        let this_is_in_guest = is_in_guest_state.unwrap_or(false);
        // A nested/re-entrant fault (this handler invoked again while an outer invocation still
        // holds this same borrow -- e.g. a genuine secondary fault occurring while already inside
        // this diagnostic block) must never panic here: `RefCell::borrow_mut`'s "already borrowed"
        // panic would itself re-enter this exception path, and a panic raised from inside Windows'
        // own exception dispatch has been observed live to loop indefinitely rather than
        // terminate, turning a real crash into an unkillable hang. Silently skip the ring update
        // on collision instead -- losing one diagnostic entry is far cheaper than masking the
        // fault behind a hang.
        RECENT_FAULTS.with(|cell| {
            if let Ok(mut ring) = cell.try_borrow_mut() {
                ring.rotate_left(1);
                ring[3] = (code, rip, rsp, is_in_guest_state);
            }
        });
        // Confirmed live (this investigation): a fault dispatched all the way down to the guest
        // shim's own "diag-guest-exception" path with `kernel_mode=false` can still have an `rip`
        // FAR outside every guest process's own address range (guest mappings observed under
        // ~0x40000000 this whole investigation; a real host-side fault here was `rip=
        // 0x7feff8ed556e`, ~128 TB, i.e. deep in Windows' high host-address region). `kernel_mode`
        // reflects x86 CPL (ring0 vs ring3), NOT "is this litebox's own host code" -- there is no
        // existing check anywhere in this dispatch path that distinguishes "genuine guest-code
        // fault" from "litebox's own host-side code faulted on a thread that happens to be
        // guest-associated", so the latter gets silently reinterpreted as a guest SIGSEGV with no
        // trace of which host function actually faulted. Resolve and log the owning module + file
        // offset for every such fault so a future capture names the exact host function instead of
        // requiring a live debugger.
        // Gated behind an explicit env var: this fires on EVERY guest-associated fault, including
        // the many expected/recoverable ones `fork_verify`'s own single-step healing deliberately
        // takes (confirmed live: unthrottled, this alone produced 480,000+ log lines and slowed
        // guest startup enough to change which bug a run even reaches -- see this investigation's
        // own notes on instrumentation distorting timing). Only pay this cost while deliberately
        // hunting a host-address/no-module fault.
        // DIAG (concurrent-fork SIGSEGV/SIGILL investigation, `rip==cr2` at instruction offsets
        // ending 0x1464b/0x464b): `LITEBOX_DIAG_FAULT_VQ=1` -- captures real Windows memory state
        // (`VirtualQuery`) at the faulting address AT THE MOMENT OF THE CRASH. AGENTS.md's "Open
        // blockers" section established the crashing instruction is a real, valid instruction at
        // the CORRECT offset relative to the child's own load base (confirmed via objdump) -- not
        // a corrupted jump target -- and that Windows genuinely reports the page as not-present,
        // not a permissions mismatch. This diagnostic answers "what does Windows' own VAD tree say
        // about this exact address right now" directly, without needing a live debugger.
        // Gated on `rip == cr2` (this investigation's own documented crash signature: an
        // instruction fetch faulting on its own address, i.e. the page backing `rip` itself is
        // not present): `fork_verify`'s own expected/recoverable single-step and AV-path
        // healing faults (the overwhelming majority of in-guest faults on this platform) do NOT
        // have `rip==cr2` -- they fault on a DIFFERENT address than the one currently executing.
        // Without this filter, this fired 268,000+ times in one 30-concurrent-fork oracle run
        // (confirmed live this investigation), each interleaved across many threads' own
        // concurrent healing traffic, making the 5 real crashes impossible to correlate back to
        // their own diagnostic block. Allocation-free (`diag_raw_print`, not `eprintln!`) and
        // still gated behind an explicit env var for the same reason `LITEBOX_DIAG_FAULT_MODULE`
        // is: unthrottled, this class of diagnostic has previously been observed to slow guest
        // startup enough to change which bug a run even reaches.
        // Widened (SIGILL/`#UD`-under-concurrent-fork investigation): the original gate below
        // (`rip == cr2`) only fires for a genuine page-fault-shaped crash where Windows reports a
        // faulting address distinct from an access-violation's `ExceptionInformation[1]` equal to
        // the current `rip`. A real `#UD` (`EXCEPTION_ILLEGAL_INSTRUCTION`) carries NO faulting
        // address at all -- `vectored_exception_handler`'s own dispatch above always reports
        // `cr2=0, error_code=0` for it (see the `Win32_Foundation::EXCEPTION_ILLEGAL_INSTRUCTION =>
        // (Exception::INVALID_OPCODE, 0, 0)` arm) -- so the strict `rip == cr2` equality never
        // holds for this class and the diagnostic silently never fires for it. Detect that case
        // directly from the raw Windows exception code instead of relying on the already-decoded
        // `cr2`, and query `rip` itself (not `cr2`, which is meaningless here) for its real
        // Windows memory-state at the moment of the fault.
        let raw_exception_code =
            unsafe { (*(*exception_info).ExceptionRecord).ExceptionCode };
        // Both `EXCEPTION_ILLEGAL_INSTRUCTION` (a real `#UD`) AND `0xc0000096`
        // (`STATUS_PRIVILEGED_INSTRUCTION`, Windows' name for an unprivileged `hlt` -- the exact
        // trap musl's mallocng `a_crash()` deliberately executes on a heap-integrity assert, see
        // this file's own dispatch match arm a few hundred lines below) both map to
        // `Exception::INVALID_OPCODE`/`SIGILL` for the guest. The original single-code check here
        // silently missed the `0xc0000096` case -- confirmed live this investigation: the real
        // dbus/`sh` SIGILL crash's own `diag-guest-exception` snapshot (downstream, in
        // `litebox_shim_linux`) fired with `exception=Exception(6)` (== INVALID_OPCODE) on both
        // observed occurrences, while THIS diagnostic never fired for either -- the only
        // explanation consistent with both facts is that the raw code took the `0xc0000096` arm,
        // not the `EXCEPTION_ILLEGAL_INSTRUCTION` arm this check alone was gated on.
        let is_ud_fault = raw_exception_code == Win32_Foundation::EXCEPTION_ILLEGAL_INSTRUCTION
            || raw_exception_code == 0xc0000096u32.cast_signed();
        // Unconditional (no env-var gate, no `this_is_in_guest` gate), allocation-free: SIGILL-
        // class investigation. `#UD` is rare enough (never fires on the ordinary hot path this
        // handler otherwise serves -- access violations and single-steps dominate call volume) to
        // print every occurrence without flooding, and this fires BEFORE the `this_is_in_guest`
        // gate below so it can independently confirm whether that gate itself is the reason the
        // main diagnostic below stays silent for this exception class.
        if is_ud_fault {
            let tid0 = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
            diag_raw_print(b"[diag-ud-entry] tid=0x", tid0 as usize, b" rip=0x", rip as usize);
            // Encode the tri-state as 0/1/2 (not `this_is_in_guest as usize`'s collapsed 0/1):
            // 2 means "no TLS slot -- could not determine", never conflated with a confirmed 0.
            diag_raw_print(
                b"[diag-ud-entry]   is_in_guest_state=0x",
                match is_in_guest_state {
                    Some(false) => 0usize,
                    Some(true) => 1usize,
                    None => 2usize,
                },
                b" raw_code=0x", raw_exception_code as usize as usize,
            );
        }
        let raw_cr2 =
            unsafe { (*(*exception_info).ExceptionRecord).ExceptionInformation[1] } as u64;
        let is_access_violation =
            raw_exception_code == Win32_Foundation::EXCEPTION_ACCESS_VIOLATION;
        if this_is_in_guest
            && veh_gates().fault_vq
            && (rip == raw_cr2 || is_ud_fault || is_access_violation)
        {
            // Widened (peer investigation, this pass): the musl dtv-clear shape this
            // investigation has chased (`mov rdx, [rax+0x80]`) is a DATA read at a perfectly
            // valid `rip`, so it satisfies neither `rip == raw_cr2` (jump-to-bad-address) nor
            // `is_ud_fault` (undefined instruction) -- this diagnostic previously stayed silent
            // for exactly the fault this investigation cares about. `is_access_violation` widens
            // it to also fire on any `EXCEPTION_ACCESS_VIOLATION` while `this_is_in_guest`,
            // regardless of `rip == cr2`, so `raw_cr2` (the actual faulting DATA address, not
            // `rip`) gets `VirtualQuery`'d and printed for this fault class too -- the decisive
            // check for whether the faulting address lies inside `HOST_ALLOCATOR_REGION_MIN`'s
            // reserved span (confirms a guest thread dereferencing host heap through a corrupted
            // FS base) versus ordinary guest/unmapped memory (kills that theory for this run).
            let cr2 = if is_ud_fault { rip } else { raw_cr2 };
            let mut cr2_mbi = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
            let cr2_queried = unsafe {
                Win32_Memory::VirtualQuery(
                    cr2 as *const c_void,
                    &raw mut cr2_mbi,
                    core::mem::size_of::<Win32_Memory::MEMORY_BASIC_INFORMATION>(),
                ) != 0
            };
            let tid = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
            diag_raw_print(b"[diag-fault-vq] tid=0x", tid as usize, b" rip=0x", rip as usize);
            diag_raw_print(b"[diag-fault-vq]   is_ud_fault=0x", is_ud_fault as usize, b" raw_code=0x", raw_exception_code as usize as usize);
            diag_raw_print(
                b"[diag-fault-vq]   cr2=0x", cr2 as usize,
                b" queried=0x", cr2_queried as usize,
            );
            diag_raw_print(
                b"[diag-fault-vq]   state=0x", cr2_mbi.State as usize,
                b" type=0x", cr2_mbi.Type as usize,
            );
            diag_raw_print(
                b"[diag-fault-vq]   protect=0x", cr2_mbi.Protect as usize,
                b" alloc_protect=0x", cr2_mbi.AllocationProtect as usize,
            );
            diag_raw_print(
                b"[diag-fault-vq]   region_base=0x", cr2_mbi.BaseAddress as usize,
                b" region_size=0x", cr2_mbi.RegionSize,
            );
            diag_raw_print(
                b"[diag-fault-vq]   alloc_base=0x", cr2_mbi.AllocationBase as usize,
                b" rsp=0x", rsp as usize,
            );
        }

        if this_is_in_guest && veh_gates().fault_module {
            let mut module: windows_sys::Win32::Foundation::HMODULE = core::ptr::null_mut();
            let resolved = unsafe {
                windows_sys::Win32::System::LibraryLoader::GetModuleHandleExW(
                    windows_sys::Win32::System::LibraryLoader::GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                        | windows_sys::Win32::System::LibraryLoader::GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                    rip as *const u16,
                    &raw mut module,
                ) != 0
            };
            if resolved && !module.is_null() {
                let mut name_buf = [0u16; 512];
                let name_len = unsafe {
                    windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW(
                        module,
                        name_buf.as_mut_ptr(),
                        name_buf.len() as u32,
                    )
                };
                let module_base = module as u64;
                let name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
                eprintln!(
                    "[diag-fault-module] rip={rip:#x} module_base={module_base:#x} module_offset={:#x} module_path={name}",
                    rip.wrapping_sub(module_base),
                );
            } else {
                let mut rip_mbi = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
                let rip_queried = unsafe {
                    Win32_Memory::VirtualQuery(
                        rip as *const c_void,
                        &raw mut rip_mbi,
                        core::mem::size_of::<Win32_Memory::MEMORY_BASIC_INFORMATION>(),
                    ) != 0
                };
                eprintln!(
                    "[diag-fault-module] rip={rip:#x} GetModuleHandleExW FAILED (rip not in any loaded module -- JIT/generated code, or a bogus address); VirtualQuery: queried={rip_queried} state={:#x} type={:#x} protect={:#x} region_base={:#x} region_size={:#x}",
                    rip_mbi.State, rip_mbi.Type, rip_mbi.Protect,
                    rip_mbi.BaseAddress as u64, rip_mbi.RegionSize,
                );
                // A control transfer landed on an address in no loaded module at all -- the
                // classic signature of a corrupted return address or a bad indirect call/jump
                // through a stale function pointer. Dump the raw stack words at `rsp` so a
                // future capture can identify the last REAL return address (one that resolves
                // to a real module) still visible on the stack, without needing a live
                // debugger attached.
                for i in 0..16u64 {
                    let slot_addr = rsp.wrapping_add(i * 8);
                    // SAFETY: best-effort diagnostic read of a small window around the faulting
                    // thread's own stack pointer; a wild/unreadable address here would itself
                    // fault, so guard with VirtualQuery (readable via a committed, non-guard,
                    // non-PAGE_NOACCESS region) rather than reading blindly.
                    let mut mbi = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
                    let queried = unsafe {
                        Win32_Memory::VirtualQuery(
                            slot_addr as *const c_void,
                            &raw mut mbi,
                            core::mem::size_of::<Win32_Memory::MEMORY_BASIC_INFORMATION>(),
                        ) != 0
                    };
                    let readable = queried
                        && mbi.State == Win32_Memory::MEM_COMMIT
                        && mbi.Protect != Win32_Memory::PAGE_NOACCESS
                        && (mbi.Protect & Win32_Memory::PAGE_GUARD) == 0;
                    if !readable {
                        eprintln!("[diag-fault-stack] rsp+{:#x}=<unreadable>", i * 8);
                        continue;
                    }
                    let word = unsafe { core::ptr::read_unaligned(slot_addr as *const u64) };
                    let mut wmod: windows_sys::Win32::Foundation::HMODULE = core::ptr::null_mut();
                    let word_resolved = unsafe {
                        windows_sys::Win32::System::LibraryLoader::GetModuleHandleExW(
                            windows_sys::Win32::System::LibraryLoader::GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                                | windows_sys::Win32::System::LibraryLoader::GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                            word as *const u16,
                            &raw mut wmod,
                        ) != 0
                    };
                    eprintln!(
                        "[diag-fault-stack] rsp+{:#x}={word:#x}{}",
                        i * 8,
                        if word_resolved && !wmod.is_null() { " (in-module)" } else { "" },
                    );
                }
            }
        }
    }

    let Some(tls) = get_tls_ptr() else {
        // TLS slot not initialized yet; cannot be in guest.
        //
        // DIAG (this investigation pass): this bail-out is one of the ways an access violation
        // with a perfectly valid, covering exception-table entry reaches
        // EXCEPTION_CONTINUE_SEARCH without the table ever being consulted -- the lookup lives
        // ~680 lines below this point. It was previously completely silent, which is precisely
        // why the "a covering entry exists but recovery never happens" paradox was so hard to
        // localise. Allocation-free and only reachable on an actual fault with no usable TLS.
        let rec = unsafe { &*(*exception_info).ExceptionRecord };
        diag_raw_print(
            b"[diag-veh-no-tls] code=0x",
            rec.ExceptionCode as usize,
            b" fault_addr=0x",
            rec.ExceptionInformation[1],
        );
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let tls = unsafe { &*tls };
    // DIAG (this investigation pass): the naked-asm trampoline's `.Lswap`/`.Lcall_here` fallback
    // (fires when `host_sp` is still null -- the narrow post-`install_tls`-pre-`run_thread_arch`
    // window every thread has -- or when `veh_depth` is at cap) calls straight into THIS function
    // on whatever stack is currently live, without ever swapping to the real host stack first.
    // Confirmed live via cdb this session: the fatal fault always has `is_verifying=false` right
    // after a `[fork_verify] end` log line, with the crashing thread's `rsp` already corrupted to
    // a near-`u64::MAX` value at the very first visible frame. If `host_sp` reads null HERE (this
    // function's own entry, reached either via the swap OR the fallback), that's this exact
    // fallback firing -- unconditional, allocation-free, so safe even if we're on an unprotected
    // guest-address stack right now.
    if tls.host_sp.get().is_null() {
        let rec = unsafe { &*(*exception_info).ExceptionRecord };
        diag_raw_print(b"[diag_null_host_sp] tid_hash=0x", std::process::id() as usize, b" code=0x", rec.ExceptionCode as usize);
    }
    if veh_gates().alloc_vec {
        let depth = tls.veh_depth.get();
        if depth > 1 {
            let rec = unsafe { &*(*exception_info).ExceptionRecord };
            let ctx = unsafe { &*(*exception_info).ContextRecord };
            diag_raw_print(b"[diag_veh_depth] REENTRANT depth=0x", depth as usize, b" code=0x", rec.ExceptionCode as usize);
            diag_raw_print(b"[diag_veh_depth]   rip=0x", ctx.Rip as usize, b" fault_addr=0x", rec.ExceptionInformation[1]);
        }
    }
    let (info, exception_record, context);
    unsafe {
        info = *exception_info;
        exception_record = &*info.ExceptionRecord;
        context = &mut *info.ContextRecord;
    }

    // TORN-READ FIX: `context` is a live pointer into the OS-owned `CONTEXT` record. Other
    // machinery in this process (`ThreadHandle::interrupt`'s `SuspendThread`/`SetThreadContext`,
    // `ctxwatch_arm_other_threads`' debug-register rewrites) can write that memory concurrently.
    // Every diagnostic below that read `context.<field>` MORE THAN ONCE in a single statement was
    // therefore capable of printing an internally inconsistent ("torn") register set -- e.g. an
    // `eprintln!` whose printed `rip` and printed `rva` (both derived from `context.Rip`) do not
    // algebraically agree via `module_base`. That is not hypothetical: it is exactly how the
    // long-chased phantom `rip=0x2e` crash signature was manufactured across many prior
    // investigation passes (AGENTS archive "pass-644" and siblings chased it directly).
    //
    // Take ONE plain struct copy up front and print from that. `CONTEXT` is `Copy` and this is a
    // fixed-size stack copy of already-valid, already-mapped memory -- no allocation, no locks, no
    // call that can itself fault, which is precisely what real crash handlers do first. It is
    // deliberately used for LOGGING ONLY: reads that feed real control flow (the `context.Rip == 0`
    // gates, `rip_in_global_allocator`, exception-table lookup) and all `context.<field> = ...`
    // writes still go through the live `context`, so this change cannot alter behaviour.
    let context_snapshot: windows_sys::Win32::System::Diagnostics::Debug::CONTEXT = *context;

    // Pass (2026-08-27 XFCE session, take 2): call the raw, allocation-free, lock-free
    // `diag_raw_regdump` (WriteFile-on-stack, same mechanism the global allocator's own
    // diagnostics use) as the UNCONDITIONAL, LITERAL FIRST thing this handler does with
    // `exception_record`/`context` in hand -- before `veh_trace_enabled()`'s own `eprintln!`
    // calls below, before `diag_fataldump_enabled()`'s gated block, before `IN_VEH_DIAG_BLOCK`.
    // Live captures this pass showed the real, causative first fault (`is_in_guest=true`,
    // `addr=usize::MAX`) reaches the `veh_trace_enabled()` block below and its `eprintln!` calls,
    // but that block's own formatting/allocation/stdio-lock machinery re-faults on this thread
    // (whose heap/lock state is already corrupted by the very bug being diagnosed) BEFORE those
    // prints complete -- permanently losing this fault's registers behind whatever LATER fault in
    // the same cascade happens to reach a working print first (previously misidentified as "the"
    // crash across three retracted hypotheses: `0x4e12c0`, `0xfefefefefefefeff`, "-libcalls" --
    // all three were actually a downstream, already-corrupted-execution-state artifact fault, not
    // the real bug). `eprintln!` is fundamentally not safe to call first on a thread in this
    // state; only a raw `WriteFile` with no allocation and no lock is trustworthy here.
    //
    // 32nd-pass fix: `diag_fataldump_enabled()` alone (LITEBOX_DIAG_FATALDUMP=1, without also
    // requesting the deliberately-expensive `veh_trace_enabled()` full instruction trace) used to
    // fire this same raw dump on EVERY exception, including EXCEPTION_SINGLE_STEP -- routine,
    // high-frequency (per-instruction) events during `fork_verify`'s own thread-based-fork
    // single-step healing, completely unrelated to what a "fatal dump" exists to catch. Live-hit
    // this pass: a fork that fell back to the thread-based path (cross-process fork ineligibility,
    // e.g. an uncarriable `unix-socket` fd) hit a single-step healing loop that never converged,
    // and `LITEBOX_DIAG_FATALDUMP=1` turned each of its thousands of single-stepped instructions
    // into a full raw register dump + `WriteFile`, flooding the log and stalling the boot for
    // minutes -- the exact "expensive diagnostic perturbs timing enough to hide the real crash"
    // failure mode this gate was originally split out to avoid, just via a different exception
    // code than the one the original split anticipated. `veh_trace_enabled()`'s own behavior is
    // UNCHANGED (it is deliberately a full per-instruction trace and single-stepping through it is
    // its whole purpose); only the `diag_fataldump_enabled()`-alone path now skips single-step.
    if veh_trace_enabled()
        || (diag_fataldump_enabled()
            && exception_record.ExceptionCode != Win32_Foundation::EXCEPTION_SINGLE_STEP)
    {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "diagnostic-only; this platform is x86_64-only, register values fit in usize"
        )]
        diag_raw_regdump(
            exception_record.ExceptionCode.cast_unsigned(),
            exception_record.ExceptionInformation[1],
            context_snapshot.Rip as usize,
            context_snapshot.Rax as usize,
            context_snapshot.Rbx as usize,
            context_snapshot.Rcx as usize,
            context_snapshot.Rdx as usize,
            context_snapshot.Rsi as usize,
            context_snapshot.Rdi as usize,
            context_snapshot.Rsp as usize,
            context_snapshot.Rbp as usize,
        );
    }

    if veh_trace_enabled() {
        // DIAG-REALSTACK (mallocng .meta=0 investigation continuation): reads the REAL host
        // TEB's StackBase/StackLimit/DeallocationStack directly via inline asm to check whether a
        // crashing thread's real Windows-backing stack reservation is genuinely the expected 8
        // MiB (`GUEST_THREAD_STACK_SIZE`) or something smaller -- e.g. because it's a `sh -c "...
        // ; weston ..."` tail-call `execve()` reusing an OS thread that was never spawned with
        // that size in the first place (`execve()` never calls `Platform::spawn_thread`, unlike
        // `fork()`/`clone()`). `StackLimit` alone is NOT sufficient here -- it tracks only the
        // currently-COMMITTED floor, which grows on demand and looks deceptively small early in a
        // thread's life; `DeallocationStack` (TEB+0x1478) is the true bottom of the whole
        // reservation, set once at thread creation, and is what actually answers the question.
        #[allow(clippy::cast_possible_truncation, reason = "diagnostic-only; x86_64 only")]
        let rsp_now = context.Rsp;
        let stack_base: u64;
        let stack_limit: u64;
        let dealloc_stack: u64;
        unsafe {
            core::arch::asm!(
                "mov {0}, gs:[0x08]",
                "mov {1}, gs:[0x10]",
                "mov {2}, gs:[0x1478]",
                out(reg) stack_base,
                out(reg) stack_limit,
                out(reg) dealloc_stack,
            );
        }
        eprintln!(
            "[veh] DIAG-REALSTACK exc_code={:#x} rsp={rsp_now:#x} stack_base={stack_base:#x} stack_limit={stack_limit:#x} dealloc_stack={dealloc_stack:#x} committed={:#x} total_reserved={:#x} remaining_to_dealloc={}",
            exception_record.ExceptionCode,
            stack_base.wrapping_sub(stack_limit),
            stack_base.wrapping_sub(dealloc_stack),
            i64::try_from(rsp_now.wrapping_sub(dealloc_stack)).unwrap_or(-1),
        );
        unsafe extern "C" {
            safe static __ImageBase: c_void;
        }
        let image_base = (&raw const __ImageBase).addr();
        eprintln!(
            "[veh] tid={:?} code={:#x} rip={:#x} rva={:#x} addr={:#x} is_in_guest={} is_verifying={} rdfsbase={:#x} thread_fs_base={:#x}",
            std::thread::current().id(),
            exception_record.ExceptionCode,
            context_snapshot.Rip,
            {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "diagnostic-only; this platform is x86_64-only, rip fits in usize"
                )]
                (context_snapshot.Rip as usize).wrapping_sub(image_base)
            },
            exception_record.ExceptionInformation[1],
            {
                // DIAG (this pass's kiosk-shell/desktop-shell shared-crash investigation): dump
                // instruction bytes at rip for large-fault-address ACCESS_VIOLATIONs too, not just
                // the near-null-pointer shape `diag_fataldump_enabled`'s own gate targets -- lets
                // this exact crash (a real, large, page-aligned fault address, e.g.
                // `0x7feffaa5e000`) be symbolized via objdump byte-matching without needing the
                // expensive full-trace mode.
                if exception_record.ExceptionCode == Win32_Foundation::EXCEPTION_ACCESS_VIOLATION
                    && exception_record.ExceptionInformation[1] >= 0x1_0000
                {
                    #[allow(clippy::cast_possible_truncation)]
                    let crip = context_snapshot.Rip as usize;
                    let mut buf16 = [0u8; 16];
                    let n = fork_verify::read_code_bytes_for_diagnostics(crip, &mut buf16);
                    eprintln!("[diag_bigfault] rip bytes ({n}): {:02x?}", &buf16[..n]);
                }
                tls.is_in_guest.get()
            },
            fork_verify::is_verifying(tls),
            unsafe { litebox_common_linux::rdfsbase() },
            WindowsUserland::get_thread_fs_base(),
        );
    }
    if diag_fataldump_enabled()
        && (exception_record.ExceptionCode == 0xC000_0096_u32.cast_signed()
            // TEMPORARY (pass 37): also dump on an ordinary access violation whose faulting
            // address is small (a near-null pointer, as opposed to an unrelated guard-page/
            // demand-paging fault at a normal-looking guest address) -- this is the shape of the
            // CI-only "segfault right after apk's own child reaps, before jq prints hello" crash
            // (`addr=0x18` observed live), which is a *data* access, not the privileged-
            // instruction/rip==0 shape the rest of this block was built for. Remove once that
            // crash is root-caused and fixed; see FINDINGS.txt pass 37.
            //
            // Pass-40: EXCLUDE the extremely common, already-diagnosed, already-repaired
            // "Windows spontaneously clears this thread's FS_BASE MSR" condition (see the long
            // comment ~40 lines below this one) -- that path also produces
            // `EXCEPTION_ACCESS_VIOLATION` with a small `ExceptionInformation[1]` (any `mov
            // %fs:offset` reads/writes through linear address `0 + offset`), fires many times per
            // second under ordinary scheduler pressure, and is unconditionally repaired and
            // retried a few lines down -- it is not fatal and was never the crash this dump exists
            // to catch. Without this exclusion the dump fired on nearly every guest instruction
            // during scheduler pressure, which is exactly the "expensive `eprintln!` perturbs
            // timing enough to hide the crash" problem `diag_fataldump_enabled` was split out to
            // avoid (confirmed empirically this pass: with the exclusion absent, a local repro run
            // that crashes in seconds under no tracing took the entire 480s budget without ever
            // reaching the real fault). Uses the EXACT same condition the repair branches below
            // use to decide whether to repair-and-retry (`rdfsbase() == 0`, `Rip != 0`, AND a
            // non-zero saved FS base to restore -- not just `rdfsbase() == 0` alone, which a first
            // pass at this exclusion used and which risked also silencing a genuine fatal fault
            // that happened to occur while FS_BASE was zero for an unrelated reason): only
            // excluded when the repair below is actually about to fire and resolve the fault,
            // never when the fault is going to reach `EXCEPTION_CONTINUE_SEARCH`/deliver a real
            // SIGSEGV to the guest.
            || (exception_record.ExceptionCode == 0xC000_0005_u32.cast_signed()
                // Xvfb-deterministic-crash pass (2026-09-21): the pass-37/42 address-magnitude
                // restriction this line used to carry (`< 0x1_0000 || == usize::MAX`) was scoped
                // to one specific prior investigation (apk/jq's near-null/GP-fault crash), not a
                // genuine overhead requirement -- the comment two screens below already
                // establishes that the ONLY high-frequency AV source is the FS_BASE-reset class,
                // and that class is excluded on its own terms by the `faulting_instruction_has_
                // fs_override` check a few lines down, independent of fault-address magnitude.
                // A live cdb capture this pass caught the SAME FS_BASE-reset class firing through
                // a large, non-near-null fault address (`fs:[r12]` register-indirect TLS access,
                // `r12=0xfffffffffffffc60`, still carrying the `0x64` FS-override prefix byte) --
                // proof the magnitude restriction was never a correct proxy for "is this the
                // repairable class", only ever a coincidence of which specific instruction shape
                // the earlier investigation happened to hit. Dropping the restriction here lets
                // this same low-overhead, in-process, event-driven dump also catch the
                // large-fault-address deterministic Xvfb SIGSEGV (`0x7feffecdd400`-shaped,
                // AGENTS.md's 31st pass) WITHOUT needing an external debugger attach at all --
                // load-bearing because attaching cdb was independently found, this same pass, to
                // freeze Xvfb's thread long enough per FS_BASE-reset dump to itself trip the
                // `xset q`/`XVFB_UP` liveness race and starve the boot of the X11 traffic volume
                // the real crash needs, making the debugger unable to observe the bug it was
                // attached to catch.
                && !(unsafe { litebox_common_linux::rdfsbase() } == 0
                    && context_snapshot.Rip != 0
                    && WindowsUserland::get_thread_fs_base() != 0
                    // Pass (DRI2/modesetting_drv.so investigation): this exclusion's whole
                    // purpose is to skip the dump for the common, auto-repaired "Windows cleared
                    // FS_BASE" condition -- but until now it used a strictly weaker check than the
                    // ACTUAL repair path a few hundred lines below (both the guest-mode and
                    // host-mode occurrences of this fix gate on
                    // `faulting_instruction_has_fs_override`, requiring the faulting instruction to
                    // genuinely carry a `0x64` FS-segment-override prefix). Without that same check
                    // here, a real, unrelated, non-FS-relative fault that merely happens to
                    // coincide with `rdfsbase() == 0` (confirmed live: a modesetting_drv.so
                    // DRI2/master-check crash with `rip` pointing at ordinary non-FS-relative code)
                    // was being silently misclassified as the benign FS_BASE case and its full
                    // diagnostic dump (register/byte dump below) was never printed -- exactly the
                    // opposite of this gate's intent, since that repair path's own `Rip != 0` guard
                    // doc comment already establishes this exact false-positive risk as the reason
                    // for requiring narrower conditions. Adding the identical
                    // `faulting_instruction_has_fs_override` check here makes the diagnostic
                    // exclusion consistent with the real repair logic: only ever excluded when this
                    // fault would ACTUALLY be repaired-and-retried, never for an unrelated fault
                    // that merely shares the same `rdfsbase()==0` signature.
                    && faulting_instruction_has_fs_override(context_snapshot.Rip.trunc()))))
        // A second exception raised while this thread is ALREADY inside this diagnostic block
        // (see `IN_VEH_DIAG_BLOCK`'s doc comment) means the diagnostic code itself is the thing
        // that just faulted -- skip straight past it and let the exception propagate normally
        // instead of recursing into the identical crash-prone path again.
        && !IN_VEH_DIAG_BLOCK.with(Cell::get)
    {
        let _veh_diag_guard = VehDiagBlockGuard::enter();
        // Pass (2026-08-27 XFCE session): print the raw crash registers FIRST, before any other
        // diagnostic call in this block runs -- prior runs showed this block can itself re-fault
        // partway through (nested exception, caught by `IN_VEH_DIAG_BLOCK` above but only AFTER
        // losing this fault's own diagnostics), so whichever fault reaches this line first now
        // always gets its registers on record even if everything after this print is lost to a
        // nested fault. Explicitly tagged FIRST/NESTED so a log with multiple faults in one
        // cascade is unambiguous about which fault produced which dump -- this exact ambiguity
        // (conflating a later cascading fault's dump with the real first fault's) cost an entire
        // prior investigation pass.
        eprintln!(
            "[veh-regs] ENTRY tid={:?} rip={:#x} rdi={:#x} rsi={:#x} rdx={:#x} rax={:#x} rsp={:#x}",
            std::thread::current().id(),
            context_snapshot.Rip,
            context_snapshot.Rdi,
            context_snapshot.Rsi,
            context_snapshot.Rdx,
            context_snapshot.Rax,
            context_snapshot.Rsp,
        );
        #[allow(
            clippy::cast_possible_truncation,
            reason = "diagnostic-only; this platform is x86_64-only, rip fits in usize"
        )]
        let rip = context_snapshot.Rip as usize;
        let mut buf = [0u8; 16];
        let n = fork_verify::read_code_bytes_for_diagnostics(rip, &mut buf);
        eprintln!("[veh] rip bytes ({n}): {:02x?}", &buf[..n]);
        let mut before = [0u8; 8];
        let before_start = rip.wrapping_sub(8);
        let nb = fork_verify::read_code_bytes_for_diagnostics(before_start, &mut before);
        eprintln!("[veh] rip-8 bytes ({nb}): {:02x?}", &before[..nb]);
        fork_verify::describe_crash_page_for_diagnostics(rip);
        let fault_addr = exception_record.ExceptionInformation[1];
        let (fa_mtype, fa_protect, fa_alloc_base) =
            fork_verify::describe_addr_for_diagnostics(fault_addr);
        eprintln!(
            "[veh] fault addr={fault_addr:#x} type={fa_mtype:#x} protect={fa_protect:#x} alloc_base={fa_alloc_base:#x} watched={}",
            fork_verify::addr_is_codewatched_for_diagnostics(fault_addr),
        );
        #[allow(
            clippy::cast_possible_truncation,
            reason = "diagnostic-only; this platform is x86_64-only, rdi fits in usize"
        )]
        let rdi = context.Rdi as usize;
        let meta_slot = rdi.wrapping_sub(0x10);
        let (ms_mtype, ms_protect, ms_alloc_base) =
            fork_verify::describe_addr_for_diagnostics(meta_slot);
        let mut meta_bytes = [0u8; 8];
        let nmb = fork_verify::read_code_bytes_for_diagnostics(meta_slot, &mut meta_bytes);
        eprintln!(
            "[veh] group meta-slot rdi-0x10={meta_slot:#x} type={ms_mtype:#x} protect={ms_protect:#x} alloc_base={ms_alloc_base:#x} bytes({nmb})={:02x?}",
            &meta_bytes[..nmb],
        );
        // Pass 132 lead (B): dump the FULL `struct group` at meta_slot (not just the 8-byte
        // `.meta` field) to distinguish "never-constructed group" (everything zero, including
        // bytes musl's own code would only ever leave nonzero) from "legitimately-retired group,
        // poisoned by `free_group()`" (per pass 131 STEP 4, musl's free.c line 30 explicitly does
        // `g->mem->meta = 0` on a genuine retirement -- ONLY that one field, nothing else in
        // `struct group`/`struct meta`). `struct group` layout (meta.h): `meta` at +0x00 (8
        // bytes, already dumped above as `meta_bytes`), `active_idx:5` bit-packed into the byte
        // at +0x08, then `pad[...]`, then `storage[]` (the actual chunk payload, i.e. what `rdi`
        // itself points at) starting at +UNIT (0x10 on this build, matching `rdi` itself). Dump
        // 0x30 bytes starting at meta_slot: covers `.meta` (+0x00..0x08), `.active_idx`+pad
        // (+0x08..0x10), and the first 0x20 bytes of `storage[]` (+0x10..0x30, i.e. what `rdi`
        // points at -- the guest chunk CPython's dict-resize free() was about to operate on) so a
        // human/future pass can see whether the surrounding bytes look like live, non-zero guest
        // heap data (supporting "group struct specifically zeroed, chunk payload untouched" i.e.
        // the free_group() poison theory) or whether the WHOLE region reads as zero (supporting
        // "this entire page/range was never populated with real data at all").
        let mut group_full = [0u8; 0x30];
        let ngf = fork_verify::read_code_bytes_for_diagnostics(meta_slot, &mut group_full);
        eprintln!(
            "[veh] group full-dump meta_slot={meta_slot:#x} (+0x00 .meta, +0x08 .active_idx/pad, +0x10.. storage[]/rdi) bytes({ngf})={:02x?}",
            &group_full[..ngf],
        );
        // Pass 129: reverse-lookup the meta-slot's PARENT-side (pre-`fork()` `duplicate()`
        // source) address and read what is still live there -- the parent's original mapping is
        // never unmapped, so this tells us whether the PARENT's own copy of this slot was
        // already zero (meaning `duplicate()` faithfully copied an already-zero value) or
        // non-zero (meaning `duplicate()`'s copy path itself lost the value).
        if let Some((source_addr, source_bytes)) =
            fork_verify::reverse_translate_and_read_for_diagnostics(tls, meta_slot)
        {
            match source_bytes {
                Some(bytes) => eprintln!(
                    "[veh] meta-slot parent-side source_addr={source_addr:#x} bytes={bytes:02x?}",
                ),
                None => eprintln!(
                    "[veh] meta-slot parent-side source_addr={source_addr:#x} READ FAILED (unmapped or unreadable, NOT a genuine zero)",
                ),
            }
        } else {
            eprintln!("[veh] meta-slot parent-side: no reverse translation found");
        }
        eprintln!(
            "[veh] crash regs rdi={:#x} rsi={:#x} rdx={:#x} rax={:#x} rsp={:#x} rbp={:#x} rbx={:#x} r8={:#x} r9={:#x} r10={:#x} r11={:#x} r12={:#x} r13={:#x} r14={:#x} r15={:#x}",
            context.Rdi,
            context.Rsi,
            context.Rdx,
            context.Rax,
            context.Rsp,
            context.Rbp,
            context.Rbx,
            context.R8,
            context.R9,
            context.R10,
            context.R11,
            context.R12,
            context.R13,
            context.R14,
            context.R15,
        );
        // DIAG (this pass's `ctx.active[]`-address investigation): `r9` at this exact crash site
        // holds the runtime address of the table `nontrivial_free`'s own disassembly indexes
        // into (a prior captured run's `r9` matched file offset 0xa4ac0 in the extracted guest
        // libc.so). Read-only probe of two candidate `struct malloc_context.active[]` base
        // hypotheses -- purely diagnostic, writes nothing.
        if veh_gates().alloc_vec {
            #[allow(clippy::cast_possible_truncation)]
            let r9 = context.R9 as usize;
            #[allow(clippy::cast_possible_truncation)]
            let sc = context.Rdx as usize;
            let candidate = r9.wrapping_add(0x50);
            diag_raw_print(b"[diag_ctx_probe] r9=0x", r9, b" sc(rdx)=0x", sc);
            #[allow(clippy::cast_possible_truncation)]
            let r8 = context.R8 as usize;
            diag_raw_print(b"[diag_ctx_probe] g(r8)=0x", r8, b" active_base=0x", candidate);
            for i in sc.saturating_sub(2)..=(sc + 2) {
                let addr = candidate.wrapping_add(i * 8);
                let mut buf8 = [0u8; 8];
                let n = fork_verify::read_code_bytes_for_diagnostics(addr, &mut buf8);
                let value = if n == 8 { usize::from_le_bytes(buf8) } else { usize::MAX };
                diag_raw_print(b"[diag_ctx_probe]   idx=0x", i, b" value=0x", value);
            }
            // Directly read g's own next/prev/mem/masks fields (struct meta layout: prev+0x0,
            // next+0x8, mem+0x10, avail_mask+0x18, freed_mask+0x1c).
            for (label_off, off) in [(0usize, 0usize), (1, 8), (2, 0x10), (3, 0x18)] {
                let addr = r8.wrapping_add(off);
                let mut buf8 = [0u8; 8];
                let n = fork_verify::read_code_bytes_for_diagnostics(addr, &mut buf8);
                let value = if n == 8 { usize::from_le_bytes(buf8) } else { usize::MAX };
                diag_raw_print(b"[diag_ctx_probe] g_field off=0x", off, b" value=0x", value);
                let _ = label_off;
            }
        }
        // Temporary diagnostic (DIAG-STACKWALK, mallocng .meta=0 investigation continuation):
        // `get_meta()` never pushes to the stack before this crash point (confirmed via
        // disassembly of the real shipped musl -- it's a leaf-shaped assert-chain using only
        // `rdi`/`rax`/`rcx`/`rdx`/`rsi`/`r8`/`r9`), so `[rsp]` at crash time should still be the
        // return address `get_meta()` will eventually `ret` to -- its caller. Widened from an
        // earlier 8-qword version: `free()`'s own prologue pushes `r14`/`rbx` and subtracts 0x18
        // from `rsp` before reaching this crash point, so `free()`'s OWN return address (its
        // caller -- the actual pixman/weston/libwayland call site that triggered this) sits
        // further up the stack than the original dump's depth reached. Dump 32 QWORDs from
        // `[rsp]` upward -- enough headroom to walk past free()'s frame and find its caller.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "diagnostic-only; this platform is x86_64-only, rsp fits in usize"
        )]
        let rsp = context.Rsp as usize;
        let mut stack_words = [0u8; 256];
        let nsw = fork_verify::read_code_bytes_for_diagnostics(rsp, &mut stack_words);
        let words: Vec<u64> = stack_words[..nsw]
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap_or([0; 8])))
            .collect();
        eprintln!("[veh] DIAG-STACKWALK rsp={rsp:#x} qwords={words:#x?}");
    }

    // Diagnostic-only (`LITEBOX_CTXWATCH=1`): decisive aliasing-vs-overwrite check at the exact
    // moment of the `rip=0` crash this whole mechanism was built to root-cause. If a hardware
    // write watchpoint was armed on this thread's `ctx.rip` field (`ctxwatch::current_armed_addr`
    // non-zero) and we just trapped with `context.Rip == 0`, read the LIVE memory currently at
    // that watched address: if it is non-zero, the watchpoint's silence (established across 70+
    // runs in prior passes: zero hits, correct address, correct cross-thread arming) is explained
    // -- nothing ever wrote 0 there, so whatever produced `rip=0` read a DIFFERENT address than
    // the one that was armed/validated (an aliasing/wrong-pointer-read bug). If it reads back 0
    // too, the field really was zeroed by a write the watchpoint should have caught but didn't,
    // pointing at a watchpoint/CPU-level gap instead.
    if context_snapshot.Rip == 0 && diag_rip0_enabled() {
        eprintln!(
            "[diag-rip0] tid={:?} exc_code={:#x} rsp={:#x} rax={:#x} is_in_guest={} is_verifying={}",
            std::thread::current().id(),
            exception_record.ExceptionCode,
            context.Rsp,
            context.Rax,
            tls.is_in_guest.get(),
            fork_verify::is_verifying(tls),
        );
        eprintln!(
            "[diag-rip0-hist] tid={:?} {}",
            std::thread::current().id(),
            diag_resume_history()
        );
        // Classify the fault. `av_type == 8` (EXCEPTION_EXECUTE_FAULT) at `av_addr == 0` means
        // the CPU faulted *fetching* an instruction at address 0, i.e. the guest itself branched
        // to null -- as opposed to LiteBox resuming the guest with a corrupted saved `rip`. The
        // words around the faulting `rsp` disambiguate further: a zero immediately below `rsp` is
        // the signature of a `ret` that popped a zeroed return address off the guest's own stack.
        {
            use core::fmt::Write as _;
            let mut around = String::new();
            for i in -4i64..4 {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "diagnostic-only; this platform is x86_64-only, so rsp fits in usize"
                )]
                let addr = (context.Rsp as usize).wrapping_add_signed(
                    isize::try_from(i * 8).expect("small constant offset fits in isize"),
                );
                let v = fork_verify::read_stack_word_for_diagnostics(addr);
                let _ = write!(around, " [rsp{i:+}*8={addr:#x}]={v:?}");
            }
            eprintln!(
                "[diag-rip0-av] tid={:?} av_type={:#x} av_addr={:#x}{}",
                std::thread::current().id(),
                exception_record.ExceptionInformation[0],
                exception_record.ExceptionInformation[1],
                around,
            );
        }
        // Pass-29: walk the faulting guest stack upward from `rsp`, classifying each word so the
        // return-address chain identifies which guest function executed the null branch. Each
        // word's own host-memory region is described via `codewatch::describe` (page type/protect/
        // alloc_base) -- a plausible return address lands in an executable, non-writable region; a
        // stack-local value lands in a writable, non-executable region; `None`/zero are called out
        // directly. 32 words covers several stack frames on a typical musl/busybox call depth.
        {
            use core::fmt::Write as _;
            let mut walk = String::new();
            for i in 0i64..32 {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "diagnostic-only; this platform is x86_64-only, so rsp fits in usize"
                )]
                let addr = (context.Rsp as usize).wrapping_add(
                    usize::try_from(i * 8).expect("small constant offset fits in usize"),
                );
                let v = fork_verify::read_stack_word_for_diagnostics(addr);
                match v {
                    None => {
                        let _ = write!(walk, "\n  [rsp+{:#06x}={:#x}] <unreadable>", i * 8, addr);
                    }
                    Some(0) => {
                        let _ = write!(walk, "\n  [rsp+{:#06x}={:#x}] = 0", i * 8, addr);
                    }
                    Some(val) => {
                        let (mtype, protect, alloc_base) =
                            fork_verify::describe_addr_for_diagnostics(val);
                        let _ = write!(
                            walk,
                            "\n  [rsp+{:#06x}={:#x}] = {val:#x}  page_type={mtype:#x} protect={protect:#x} alloc_base={alloc_base:#x}",
                            i * 8,
                            addr,
                        );
                    }
                }
            }
            eprintln!(
                "[diag-rip0-stackwalk] tid={:?} rsp={:#x}{}",
                std::thread::current().id(),
                context.Rsp,
                walk,
            );
        }
        // Pass-30: record the faulting rsp so the reactive watchpoint (armed further below, AFTER
        // `ctxwatch::disarm()` runs on this same VEH call as the crash is absorbed into a
        // guest-visible SIGSEGV) targets `faulting_rsp - 8` -- see that arm site's own comment.
        diag_pending_watch_addr(context.Rsp);
    }
    if ctxwatch::enabled() && context_snapshot.Rip == 0 {
        let watched = ctxwatch::current_armed_addr();
        if watched != 0 {
            let live_value = unsafe { core::ptr::read_unaligned(watched as *const u64) };
            eprintln!(
                "[ctxwatch] CRASH tid={:?} rip=0 watched_addr={:#x} live_value_at_watched_addr={:#x}",
                std::thread::current().id(),
                watched,
                live_value,
            );
        } else {
            eprintln!(
                "[ctxwatch] CRASH tid={:?} rip=0 but no watchpoint currently armed on this thread",
                std::thread::current().id(),
            );
        }
    }

    // Diagnostic code-page watchpoint (`LITEBOX_CODEWATCH=1`). Both of these must be triaged
    // BEFORE the `!is_in_guest` branch below: the whole point of the watchpoint is to observe
    // writes into a `fork()` child's own copied code that happen while LiteBox's *host*
    // syscall-servicing code is running (i.e. `is_in_guest == false`), which that branch would
    // otherwise hand straight to `EXCEPTION_CONTINUE_SEARCH` and turn into a process-killing
    // unhandled exception -- as would the `TF` single-step the watchpoint uses to let the trapped
    // write complete.
    if exception_record.ExceptionCode == Win32_Foundation::EXCEPTION_ACCESS_VIOLATION
        && fork_verify::on_codewatch_write(exception_record, context)
    {
        return EXCEPTION_CONTINUE_EXECUTION;
    }
    if exception_record.ExceptionCode == Win32_Foundation::EXCEPTION_SINGLE_STEP
        && !tls.is_in_guest.get()
        && fork_verify::on_codewatch_step(context)
    {
        return EXCEPTION_CONTINUE_EXECUTION;
    }

    // Diagnostic-only (`LITEBOX_CTXWATCH=1`): a hardware write-watchpoint hit on some thread's
    // `ctx.rip` field. Checked regardless of `is_in_guest` -- the whole point is to catch a
    // WRITER thread that may be running host code, not necessarily the watched thread's own
    // guest execution (which never legitimately writes this field via the watched path at all).
    // Resume immediately after logging; the write already completed (hardware data watchpoints
    // trap post-write), so there is nothing further to step over.
    if exception_record.ExceptionCode == Win32_Foundation::EXCEPTION_SINGLE_STEP
        && ctxwatch::on_possible_hit(context)
    {
        return EXCEPTION_CONTINUE_EXECUTION;
    }

    // Temporary (see FINDINGS.txt PASS 48): `LITEBOX_DIAG_WATCHADDR=<hex>`-gated fixed-address
    // `Dr1` write watch, independent of the `Dr0`-based mechanism above -- see
    // `ctxwatch::arm_fixed_on_current_thread`'s doc comment.
    if exception_record.ExceptionCode == Win32_Foundation::EXCEPTION_SINGLE_STEP
        && ctxwatch::on_possible_fixed_hit(context)
    {
        return EXCEPTION_CONTINUE_EXECUTION;
    }

    if !tls.is_in_guest.get() {
        // Same FS_BASE-reset repair as the guest-mode case below, but for *host* Rust code: live
        // tracing (`LITEBOX_VEH_TRACE=1`) while investigating an `apk add nodejs` trigger-script
        // hang showed `EXCEPTION_ACCESS_VIOLATION`s with `is_in_guest == false` and
        // `rdfsbase() == 0` -- the same signature as the guest-mode case, just reached while
        // running host code between guest instructions instead of guest code. Repairing it here
        // too, before the exception-table lookup below, is a real, verified improvement (confirmed
        // firing correctly in that trace).
        //
        // A second, now more precisely characterized, and still separate issue remains open past
        // this fix (and past the `EXCEPTION_SINGLE_STEP`-path FS_BASE repair below, which fixes
        // the quadratic single-step/FS_BASE-reset slowdown this comment originally attributed the
        // whole hang to): `apk add --no-cache nodejs` deterministically stalls forever partway
        // through -- specifically while `ash` runs the `icu-data-en` package's `.post-install`
        // trigger script, right at the point that script's `fork()`ed child completes its
        // `execve()` (the last traced activity is always the fork_verify single-step window's
        // final few guest instructions immediately before the call into `switch_to_guest`/the
        // syscall trampoline for `execve`; nothing more is ever traced afterward on any thread).
        // `gdb`-attaching to a stalled process shows every OS thread cleanly parked in
        // `WaitOnAddress`/`recvfrom`/threadpool-wait -- no thread spinning, no thread executing
        // guest code, no panic message on stderr -- consistent with the parent's `wait4()` (via
        // `Process::wait_for_exit`'s `nr_threads.block(n)` loop) never being woken because
        // `Process::detach_thread`'s "last thread exited" bookkeeping and wake never ran for the
        // execve'd child, rather than any FS_BASE or FS_BASE-adjacent problem (`rdfsbase()` reads
        // back correct at every point observed near the stall). Notably, attaching `gdb` (whose
        // `attach` implicitly suspends every thread in the process, the same primitive
        // `ThreadHandle::interrupt` uses via `SuspendThread`/`SetThreadContext`/`ResumeThread`)
        // was observed to occasionally produce one more increment of forward progress before the
        // process re-stalled identically -- suggestive of a race in the interrupt/thread-exit
        // signaling path (`litebox/src/event/wait.rs`, `Process::detach_thread`) rather than a
        // true unconditional infinite loop, but not yet root-caused to a specific line. This is a
        // distinct bug from the FS_BASE/single-step quadratic slowdown fixed in this commit and
        // deserves its own dedicated investigation (ideally starting from a debug build under
        // `gdb` with breakpoints on `Process::detach_thread`/`ThreadHandle::interrupt`, reproduced
        // via `apk add --no-cache nodejs` against a freshly packaged `alpine-rootfs.tar`) rather
        // than being folded into this fix.
        if exception_record.ExceptionCode == Win32_Foundation::EXCEPTION_ACCESS_VIOLATION
            && veh_gates().avfull
        {
            // `has_fs_override` reads the instruction bytes AT `rip` -- when `rip` itself is a
            // corrupted, unmapped address (the `rip=0x100000001`-class crash this diagnostic was
            // built to characterize), there is no instruction to read at all, so
            // `faulting_instruction_has_fs_override` correctly (and, per its own doc comment,
            // intentionally) returns `false` as a safe false-negative for the FS_BASE-repair
            // guard's purposes below. That same `false` is misleading read as a standalone
            // diagnostic line, though: it looks identical to "confirmed not FS-relative" when the
            // real state is "rip is unreadable, this tells us nothing" -- confirmed live this
            // session, where a `has_fs_override=false` reading for a corrupted `rip` initially
            // looked like it ruled out the FS-relative-canary-check pattern, until a gdb-attached
            // repro of the same crash class showed the actual fault (before `rip` got corrupted
            // further downstream) was a completely ordinary `sub %fs:0x28,%rax` stack-protector
            // check. Report whether `rip` was even readable as its own field so a future
            // investigator sees "unreadable, inconclusive" rather than a false "not FS-relative".
            let mut probe = [0u8; 4];
            let rip_readable =
                fork_verify::read_code_bytes_for_diagnostics(context_snapshot.Rip.trunc(), &mut probe) > 0;
            eprintln!(
                "[diag-avfull] tid={:?} rip={:#x} fsbase={:#x} fault_addr={:#x} rip_readable={} has_fs_override={}",
                std::thread::current().id(),
                context_snapshot.Rip,
                unsafe { litebox_common_linux::rdfsbase() },
                exception_record.ExceptionInformation[1],
                rip_readable,
                faulting_instruction_has_fs_override(context_snapshot.Rip.trunc()),
            );
        }
        if exception_record.ExceptionCode == Win32_Foundation::EXCEPTION_ACCESS_VIOLATION
            && unsafe { litebox_common_linux::rdfsbase() } == 0
            // A zero `Rip` is not a real FS_BASE-reset fault: the FS_BASE-reset repair's whole
            // premise is that the guest/host instruction at `Rip` is genuine and merely read/wrote
            // through the wrong (zeroed) segment base -- retrying it after `wrfsbase` makes forward
            // progress. `Rip == 0` means the CPU never reached a real instruction at all, so
            // "repairing" FS_BASE and resuming at address 0 just re-faults with the exact same
            // signature (`EXCEPTION_ACCESS_VIOLATION`, `rdfsbase() == 0`, because execution never
            // gets anywhere real to leave FS_BASE in a consistent state) -- an infinite repair loop
            // observed in practice via `LITEBOX_VEH_TRACE=1` (1809+ repeated repairs, no forward
            // progress). Skip the repair here so this falls through to the exception-table lookup /
            // `EXCEPTION_CONTINUE_SEARCH` below instead, turning the silent livelock into a
            // diagnosable crash.
            //
            // TORN-READ FIX (track-b sweep): uses `context_snapshot.Rip`, not a live re-read of
            // `context.Rip` -- see this function's own "TORN-READ FIX" comment above
            // `context_snapshot`'s definition. This is a real control-flow gate (decides whether
            // the FS_BASE repair below fires), not a diagnostic, so a torn value here can
            // misroute a genuinely repairable fault into the fatal/unrecovered path.
            && context_snapshot.Rip != 0
            // The fault must actually be an FS-relative access -- see
            // `faulting_instruction_has_fs_override`'s doc comment for why this guard exists: an
            // unrelated real fault (e.g. a null-pointer dereference with no FS prefix at all)
            // coinciding with `rdfsbase() == 0` was being misdiagnosed as FS_BASE-reset and
            // retried forever, since `wrfsbase` does nothing to fix a fault that was never about
            // FS_BASE in the first place.
            && faulting_instruction_has_fs_override(context_snapshot.Rip.trunc())
        {
            let saved = WindowsUserland::get_thread_fs_base();
            if saved != 0 {
                if veh_trace_enabled() {
                    eprintln!(
                        "[veh] tid={:?} host-mode FS_BASE-reset in-place repair (rip={:#x})",
                        std::thread::current().id(),
                        context_snapshot.Rip,
                    );
                }
                // Confirmed live via `cdb`-attached exception-record capture (AGENTS.md pass
                // 302): under a sufficiently high FS_BASE-reset rate, Windows can clear the MSR
                // again between this write and the retried instruction actually completing, so a
                // single `wrfsbase` is not always enough. Loop a small, bounded number of times,
                // re-checking `rdfsbase()` after each write, before resuming -- this only costs
                // extra work in the rare case the first write already lost the race.
                for _ in 0..8 {
                    unsafe { litebox_common_linux::wrfsbase(saved) };
                    if unsafe { litebox_common_linux::rdfsbase() } == saved {
                        break;
                    }
                }
                return EXCEPTION_CONTINUE_EXECUTION;
            }
        }

        // This might be a faulting guest memory access in LiteBox code. Try to
        // recover.
        //
        // TORN-READ FIX (root cause, track-b investigation): this call MUST search on
        // `context_snapshot.Rip`, not a live re-read of `context.Rip`. `context` is a pointer into
        // the OS-owned in-flight `CONTEXT` record, which -- per this function's own "TORN-READ FIX"
        // comment above `context_snapshot`'s definition -- other machinery in this process
        // (`ThreadHandle::interrupt`'s `SuspendThread`/`SetThreadContext`, `ctxwatch_arm_other_
        // threads`' debug-register rewrites) can write concurrently. A live capture (this
        // investigation) proved this is not hypothetical here either: `search_exception_tables` was
        // observed being invoked with `context.Rip` already equal to the fault's *memory* address
        // (`ExceptionInformation[1]`/`Rdx`/`Rsi`, e.g. `0x10188000`) rather than the instruction
        // pointer, while `context_snapshot.Rip` -- captured once, immediately on entry, before any
        // of this function's own logic runs -- still held the correct, in-table-covered `rip`
        // (`0x7ff6d80efad8`, confirmed by the sibling unrecovered-branch diagnostic's own
        // `debug_snapshot_table` dump to have `covers=true` for entry `[3]`). Searching on the live,
        // racing `context.Rip` therefore fed `search_exception_tables` a value that never belonged
        // to this fault's instruction pointer at all, guaranteeing a spurious `None` and diverting a
        // genuinely recoverable AV into the fatal `[diag-unrecov-av]` path. `context_snapshot` was
        // already captured for exactly this reason (see its own doc comment); this call was simply
        // never updated to use it.
        if exception_record.ExceptionCode == Win32_Foundation::EXCEPTION_ACCESS_VIOLATION
            && let Some(recover) =
                litebox::mm::exception_table::search_exception_tables(
                    context_snapshot.Rip.trunc(),
                )
        {
            // Found a matching exception table entry.
            //
            // Unconditional (not gated on `veh_trace_enabled()`): captures the EXACT `recover`
            // (fixup) address the exception table redirects `Rip` to, so a future capture can
            // directly disassemble it and check whether its own surrounding assumptions (register
            // state, stack alignment) hold when entered via an injected `Rip` write rather than a
            // normal in-function jump -- see this investigation's own "unified epilogue" finding.
            // `try_borrow_mut`: see the sibling `RECENT_FAULTS` update's comment above -- a
            // nested/re-entrant fault must never panic on an already-held borrow here.
            RECOVERY_LOG.with(|cell| {
                if let Ok(mut ring) = cell.try_borrow_mut() {
                    ring.rotate_left(1);
                    ring[3] = (context_snapshot.Rip, recover as u64);
                }
            });
            // DIAG (pass 205 follow-up): the fatal-fault investigation (AGENTS.md pass 205)
            // proved this recovered branch IS taken for the specific `memset_fallible` AV that
            // precedes the mysterious constant-address `c000000d` secondary fault -- but had no
            // visibility into the recovered page's real Windows commit/protect state at the
            // moment of recovery, nor RSP's 16-byte alignment right before returning
            // `EXCEPTION_CONTINUE_EXECUTION`. Unconditional (not gated on `veh_trace_enabled()`,
            // matching the sibling unrecovered-branch diagnostic below): cheap (one
            // `VirtualQuery`, no allocation on the hot path since this only runs on an actual
            // fault, never on ordinary execution), and this is exactly the path the whole
            // investigation has never been able to observe directly.
            if diag_fataldump_enabled() {
                // Allocation-free (`diag_raw_print`, not `eprintln!`): pass 205's own capture
                // showed this branch's original `eprintln!`-based version printed NOTHING despite
                // running before `context.Rip` is overwritten -- i.e. even reaching the
                // `eprintln!`/formatting machinery is enough to lose the print, matching this
                // file's own established pattern (see `diag_raw_regdump`'s doc comment) that
                // allocation-based printing is not trustworthy this early/this deep in a fault
                // this investigation is chasing. Two raw prints instead of one richer `eprintln!`
                // line, since `diag_raw_print` only carries two hex values at a time.
                let fault_addr = exception_record.ExceptionInformation[1] as usize;
                let mut mbi = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
                let ok = unsafe {
                    Win32_Memory::VirtualQuery(
                        fault_addr as *mut c_void,
                        &mut mbi,
                        core::mem::size_of::<Win32_Memory::MEMORY_BASIC_INFORMATION>(),
                    ) != 0
                };
                diag_raw_print(
                    b"[diag-recovered-av] fault_addr=0x",
                    fault_addr,
                    b" recover_rip=0x",
                    recover as usize,
                );
                diag_raw_print(
                    b"[diag-recovered-av2] rsp=0x",
                    context.Rsp as usize,
                    b" State=0x",
                    if ok { mbi.State as usize } else { 0xdead },
                );
                // The FS base AT THE MOMENT OF RECOVERY, and the value this thread should have.
                //
                // The fixup this branch is about to jump to is not a bare `jmp`: `write_fn!`
                // declares it as `fault = label { return Err(Fault) }` (see
                // `litebox::mm::exception_table`), i.e. real Rust that returns through the
                // function's own epilogue. Any stack-protector cookie load, TLS access or
                // `__chkstk` probe on that path is FS-relative -- so resuming there while Windows
                // has zeroed FS_BASE (the documented condition this whole VEH exists to repair)
                // would fault immediately at a small offset from zero. That is exactly the
                // observed `addr=0x0`, host-side, inside `write_u32_fallible`. Note the sibling
                // FS_BASE-reset branch above DOES `wrfsbase` before resuming; this branch does
                // not, which is the asymmetry this print exists to confirm or refute.
                diag_raw_print(
                    b"[diag-recovered-av3] rdfsbase=0x",
                    unsafe { litebox_common_linux::rdfsbase() } as usize,
                    b" saved_fs=0x",
                    WindowsUserland::get_thread_fs_base(),
                );
            }
            // DIAG (this investigation): the sibling FS_BASE-reset branch immediately above does
            // real repair work (`wrfsbase` + a bounded verify loop) before resuming, but THIS
            // branch resumes at `recover` with no FS_BASE handling at all. `recover` is the
            // compiler-generated `Err(Fault)` fixup block inside a real Rust function, whose
            // epilogue can perform FS-relative accesses (stack-protector / TLS). If FS_BASE reads
            // back as 0 here, resuming at `recover` would fault again at a small offset from zero
            // -- exactly the `addr=0x0`, host-side, `is_in_guest=false` signature this
            // investigation captured. Ungated and allocation-free: this only runs on an actual
            // recovered fault, never on ordinary execution, and the gated `diag_fataldump_enabled`
            // block above is not on by default in the runs that reproduce this crash.
            //
            // FIX (track-b investigation, confirmed live): the comment above predicted, and a
            // live `bash -c` fork repro under `LITEBOX_PROCESS_FORK=1` then actually hit,
            // `rdfsbase()==0` at exactly this point -- FS_BASE cleared by Windows, about to
            // resume at `recover`'s FS-relative stack-protector/TLS epilogue code with no
            // repair. That produced a silent hang (no further log output, process alive but
            // idle) rather than a clean crash, because the re-fault this causes does not
            // reliably match the same guarded conditions (`faulting_instruction_has_fs_override`
            // plus the sibling branch's placement earlier in this function) on every retry.
            // Apply the exact same bounded `wrfsbase`-and-verify repair the sibling
            // FS_BASE-reset branch above already uses, using the same trusted
            // `THREAD_FS_BASE`-shadowed value (never repairing to an untrusted 0), before
            // resuming -- so `recover`'s epilogue observes a real FS_BASE instead of faulting
            // again immediately.
            let saved_for_recover = WindowsUserland::get_thread_fs_base();
            if saved_for_recover != 0 && unsafe { litebox_common_linux::rdfsbase() } == 0 {
                for _ in 0..8 {
                    unsafe { litebox_common_linux::wrfsbase(saved_for_recover) };
                    if unsafe { litebox_common_linux::rdfsbase() } == saved_for_recover {
                        break;
                    }
                }
            }
            diag_raw_print(
                b"[diag-recover-fsbase] recover_rip=0x",
                recover as usize,
                b" fsbase=0x",
                unsafe { litebox_common_linux::rdfsbase() } as usize,
            );
            // Track-B investigation (fork-without-exec hang): arm the same fault-terminate
            // watchdog used by the unconditional-terminate path below, but for a DIFFERENT
            // reason -- this resume itself is the moment live evidence this session pinpointed as
            // the actual hang trigger. Five consecutive deterministic `LITEBOX_PROCESS_FORK=1`
            // repros (reproduced with no debugger ever attached, and reproduced identically with
            // Windows Error Reporting fully disabled) showed the guest thread print exactly this
            // `[diag-recover-fsbase]` line with a healthy, non-zero `fsbase`, then go completely
            // silent -- CPU pinned at 0%, `WaitReason=Suspended` -- with NO further log output
            // and, critically, NO subsequent re-entry into `vectored_exception_handler` at all
            // (confirmed: `[diag-unrecov-av-terminate]`/`[diag-unrecov-av]` never appear in the
            // affected runs' captured logs), meaning the unconditional-terminate arm a few lines
            // below this one in the source never gets a chance to fire for this specific case --
            // the thread does not visibly fault again, it simply never resumes. `context.Rip =
            // recover` below writes an arbitrary resume target with no corresponding `call`
            // instruction (an `NtContinue`-driven synthetic control transfer); `recover` is
            // itself compiler-generated fixup code whose own epilogue can raise a SECOND
            // exception this VEH may never observe as a normal AV re-entry (this host's ntdll
            // build, confirmed via a real crash this exact process previously logged with
            // exception code `0xc0000409`/`STATUS_STACK_BUFFER_OVERRUN` at a fixed offset,
            // suggests a fast-fail-class kernel exception is the more likely mechanism than an
            // ordinary AV).
            //
            // The watchdog is NO LONGER ARMED HERE, and the comment above is kept because its
            // evidence is still the record of why it once was.
            //
            // Arming on this path was a process-wide death sentence for an ordinary, successful
            // recovery. `FAULT_TERMINATE_ARMED_TICK` is a monotonic counter that nothing ever
            // resets, and `fault_terminate_watchdog_thread_body` treats any non-zero value as
            // "armed" -- it only clears its tick count when the counter reads exactly 0. So the
            // FIRST exception-table recovery in a process's life armed the watchdog permanently,
            // and three seconds later it killed the process, whether or not anything was actually
            // wrong. Every other arming site is on a terminate path, where a monotonic
            // never-reset counter is exactly right because the process is already ending; this one
            // was on a path whose whole purpose is to carry on.
            //
            // The safety-check sentence above is also no longer true: that CPU-delta check was
            // removed as live-confirmed unreliable (see `fault_terminate_watchdog_thread_body`),
            // leaving nothing between a recovery and a kill but a 3-second grace period.
            //
            // Measured: `mate-session` logged `[diag-recover-fsbase]` three times -- three
            // successful recoveries -- and then `[diag-fault-watchdog-terminate]`. With the
            // watchdog disabled the identical run completes cleanly, exit 0, zero unrecoverable
            // AVs, `xdotool` reporting a live `mate-session` window. It was never wedged.
            //
            // The hang this arming was added to catch is very likely the same corruption fixed by
            // `VEH_FRAME_STRIDE`'s sizing (see that constant): a nested handler frame overlapping
            // a live outer one produced exactly the "resumes at `recover` and is never heard from
            // again" signature. If a genuine recovery-resume hang ever returns, it must be caught
            // by something that can tell a wedged process from a working one -- which a counter
            // that only ever counts up cannot.
            context.Rip = recover as u64;
            return EXCEPTION_CONTINUE_EXECUTION;
        } else {
            // Not one of our exceptions; let other handlers process it.
            //
            // Unconditional (not gated on `veh_trace_enabled()`, unlike the sibling recovery
            // branch above): this is the rare, crash-relevant path -- a genuine unrecovered AV in
            // host code -- and printing it costs nothing on the (overwhelmingly common) path
            // where no exception ever fires. `LITEBOX_VEH_TRACE=1`'s own tracing overhead has
            // been observed to change this bug's timing enough to mask it entirely (a real
            // access-violation-class fault racing a fork-heavy repro), so this diagnostic exists
            // specifically to survive on the fast/untraced path where the crash actually occurs.
            if exception_record.ExceptionCode == Win32_Foundation::EXCEPTION_ACCESS_VIOLATION {
                // AGENTS.md pass 232: a genuinely unrecovered AV at this exact point (most
                // often the still-unexplained `ntdll!RtlpUnwindPrologue` fault documented since
                // pass 205) has been observed live to recur at the SAME `rip`, thousands of
                // times per second, forever -- `EXCEPTION_CONTINUE_SEARCH` below hands the fault
                // back to Windows, which apparently just re-delivers the identical fault
                // instead of ever terminating the process or reaching a different handler.
                // Confirmed live during a real XFCE session (pass 230): the log grew past
                // 500,000 lines in a few seconds with the host process still nominally "alive"
                // but making zero forward progress, an unbounded resource-exhaustion hazard
                // (disk space) with no natural end. Bound this: if the SAME `rip` faults this
                // way more than a small, generous number of times in a row on one thread,
                // conclude this is genuinely unrecoverable and terminate the WHOLE process
                // cleanly via `TerminateProcess` rather than let Windows spin forever -- this
                // sacrifices whatever this one thread was doing (already true today, just via
                // an infinite hang instead of a clean exit) without risking the runaway
                // disk-exhaustion failure mode observed live.
                thread_local! {
                    static LAST_UNRECOV_AV: core::cell::Cell<(u64, u32)> =
                        const { core::cell::Cell::new((0, 0)) };
                }
                const MAX_REPEATED_UNRECOV_AV: u32 = 64;
                // TORN-READ FIX (track-b sweep): `context_snapshot.Rip`, not a live re-read of
                // `context.Rip` -- this repeat-count comparison directly gates whether the
                // process self-terminates via `TerminateProcess`/`RaiseFailFastException` below,
                // so a torn value here can either falsely reset the counter (masking a genuine
                // livelock forever) or falsely advance it (terminating on unrelated faults that
                // merely raced this field).
                let (last_rip, repeat_count) = LAST_UNRECOV_AV.get();
                let repeat_count = if last_rip == context_snapshot.Rip {
                    repeat_count + 1
                } else {
                    1
                };
                LAST_UNRECOV_AV.set((context_snapshot.Rip, repeat_count));
                if repeat_count > MAX_REPEATED_UNRECOV_AV {
                    // Diagnostic escape hatch: `TerminateProcess` exits cleanly and never
                    // reaches Windows Error Reporting, so WER's LocalDumps (configured
                    // out-of-band for this investigation) never captures a minidump of the
                    // actual fault. Setting LITEBOX_DIAG_ALLOW_WER=1 skips this clean exit for
                    // exactly this one repeated-fault path and instead falls through to
                    // EXCEPTION_CONTINUE_SEARCH below, letting the real unhandled exception
                    // reach Windows so WER can capture full register/stack state. Never set
                    // this outside a debugging session -- it reintroduces the unbounded
                    // disk-exhaustion hazard this circuit breaker exists to prevent.
                    if !veh_gates().allow_wer {
                        diag_raw_print(
                            b"[diag-unrecov-av-giveup] rip=0x",
                            context_snapshot.Rip as usize,
                            b" repeat_count=0x",
                            repeat_count as usize,
                        );
                        // See `FAULT_TERMINATE_ARMED_TICK`'s doc comment: this same-thread
                        // `TerminateProcess` call has live evidence of not always completing on
                        // its own -- arm the watchdog first as a backstop, same as the sibling
                        // unconditional-terminate path below.
                        FAULT_TERMINATE_ARMED_TICK.fetch_add(1, Ordering::SeqCst);
            // Capture a real minidump now that the watchdog is standing by. Deliberately AFTER the
            // arm, never before: `MiniDumpWriteDump` walks every thread in the process and can
            // block, so if it never returns the watchdog still terminates -- the dump attempt
            // cannot turn a crash into a hang. See `write_crash_minidump` for why this uses the
            // native API rather than a crate, and what it deliberately does not capture.
            write_crash_minidump(exception_info);
                        unsafe {
                            windows_sys::Win32::System::Threading::TerminateProcess(
                                windows_sys::Win32::System::Threading::GetCurrentProcess(),
                                1,
                            );
                        }
                    } else {
                        // `EXCEPTION_CONTINUE_SEARCH` was tried first and found to recurse back
                        // into this SAME VEH on immediate re-delivery of the identical fault,
                        // exhausting the thread's stack ("has overflowed its stack") before ever
                        // reaching a WER-visible unhandled-exception path. `RaiseFailFastException`
                        // is the direct, WER-compatible fail-fast path Windows itself uses for
                        // unrecoverable corruption (e.g. heap corruption, __fastfail) -- it invokes
                        // crash reporting immediately with the CURRENT context, no re-delivery, no
                        // handler chain to recurse through.
                        diag_raw_print(
                            b"[diag-unrecov-av-allow-wer] rip=0x",
                            context_snapshot.Rip as usize,
                            b" repeat_count=0x",
                            repeat_count as usize,
                        );
                        unsafe {
                            windows_sys::Win32::System::Diagnostics::Debug::RaiseFailFastException(
                                exception_record as *const EXCEPTION_RECORD,
                                context as *const _
                                    as *const windows_sys::Win32::System::Diagnostics::Debug::CONTEXT,
                                0,
                            );
                        }
                    }
                }
                // Ungated, allocation-free: the trampoline bails out to `.Lsearch`
                // (EXCEPTION_CONTINUE_SEARCH, Rust never entered, exception table never
                // consulted) once `veh_depth` reaches `VEH_DEPTH_CAP`, and that bail-out is
                // completely silent today. A fault cascade that climbs toward the cap therefore
                // stops being recoverable partway through, with no trace of why -- exactly the
                // "a covering entry exists but recovery never happens" shape this investigation
                // is chasing. This file's own trampoline comment records a live repro hitting
                // 3000+ nested reentries, well past the 512 cap, so this is not hypothetical.
                // Print the depth reached on this invocation so a capture shows directly whether
                // the cascade is depth-driven.
                diag_raw_print(
                    b"[diag-unrecov-av-depth] veh_depth=0x",
                    tls.veh_depth.get() as usize,
                    b" cap=0x",
                    VEH_DEPTH_CAP as usize,
                );
                // Companion to the depth print: how many faults have taken the trampoline's
                // invisible `.Lsearch` exit process-wide (see `LSEARCH_EXIT_COUNT`). A non-zero
                // count here means faults ARE bypassing the exception table entirely without
                // Rust ever running -- the mechanism that would make a covering entry look like
                // it simply failed to recover.
                diag_raw_print(
                    b"[diag-lsearch-exits] count=0x",
                    LSEARCH_EXIT_COUNT.load(Ordering::Relaxed) as usize,
                    b" veh_depth=0x",
                    tls.veh_depth.get() as usize,
                );
                unsafe extern "C" {
                    safe static __ImageBase: c_void;
                }
                let module_base = (&raw const __ImageBase) as usize;
                // The on-disk `.extable` section of this very binary was verified to contain an
                // entry whose [start, stop) range covers this exact faulting RVA, yet the lookup
                // above reported no match. Print the table the process ACTUALLY walks, relocated
                // into live addresses, so the two can be compared directly instead of assumed
                // identical. Fixed-size stack buffer, no allocation -- safe inside this handler.
                {
                    let mut entries = [(0usize, 0usize, 0usize); 32];
                    let n = litebox::mm::exception_table::debug_snapshot_table(&mut entries);
                    eprintln!(
                        "[diag-extable] module_base={module_base:#x} rip={:#x} rva={:#x} table_len={} shown={n}",
                        context_snapshot.Rip,
                        (context_snapshot.Rip as usize).wrapping_sub(module_base),
                        litebox::mm::exception_table::debug_table_len(),
                    );
                    for (i, (s, e, f)) in entries.iter().take(n).enumerate() {
                        let covers = (context_snapshot.Rip as usize) >= *s
                            && (context_snapshot.Rip as usize) < *e;
                        eprintln!(
                            "[diag-extable]   [{i}] start={s:#x} (rva {:#x}) stop={e:#x} (rva {:#x}) fixup={f:#x} covers={covers}",
                            s.wrapping_sub(module_base),
                            e.wrapping_sub(module_base),
                        );
                    }
                }
                eprintln!(
                    "[diag-unrecov-av] tid={:?} rip={:#x} rva={:#x} addr={:#x} rsp={:#x} rax={:#x} rbx={:#x} rcx={:#x} rdx={:#x} rsi={:#x} rdi={:#x} rbp={:#x} is_in_guest={} is_verifying={} -- no exception-table entry found",
                    std::thread::current().id(),
                    context_snapshot.Rip,
                    (context_snapshot.Rip as usize).wrapping_sub(module_base),
                    exception_record.ExceptionInformation[1],
                    context_snapshot.Rsp,
                    context_snapshot.Rax,
                    context_snapshot.Rbx,
                    context_snapshot.Rcx,
                    context_snapshot.Rdx,
                    context_snapshot.Rsi,
                    context_snapshot.Rdi,
                    context_snapshot.Rbp,
                    tls.is_in_guest.get(),
                    fork_verify::is_verifying(tls),
                );
                // Query the REAL Windows page state of the exact faulting address at the moment
                // of the fault -- directly tests the 41st-pass hypothesis that the address itself
                // is legitimately computed but the underlying page is not actually committed (or
                // was committed then silently decommitted/relocated by another thread) despite
                // the mapping call that should have committed it having already reported success.
                {
                    // Identify which GPR (if any) exactly matches the fault address -- narrows
                    // down which register carries the corrupted/wild value that produced this
                    // fault, without needing full per-instruction register-history tracing.
                    let fault_addr_val = exception_record.ExceptionInformation[1] as u64;
                    let matches: alloc::vec::Vec<&str> = [
                        ("rax", context_snapshot.Rax),
                        ("rbx", context_snapshot.Rbx),
                        ("rcx", context_snapshot.Rcx),
                        ("rdx", context_snapshot.Rdx),
                        ("rsi", context_snapshot.Rsi),
                        ("rdi", context_snapshot.Rdi),
                        ("rbp", context_snapshot.Rbp),
                        ("rsp", context_snapshot.Rsp),
                        ("r8", context_snapshot.R8),
                        ("r9", context_snapshot.R9),
                        ("r10", context_snapshot.R10),
                        ("r11", context_snapshot.R11),
                        ("r12", context_snapshot.R12),
                        ("r13", context_snapshot.R13),
                        ("r14", context_snapshot.R14),
                        ("r15", context_snapshot.R15),
                    ]
                    .iter()
                    .filter(|(_, v)| *v == fault_addr_val)
                    .map(|(name, _)| *name)
                    .collect();
                    eprintln!(
                        "[diag-unrecov-av-gprmatch] fault_addr={:#x} matching_gprs={:?}",
                        fault_addr_val, matches,
                    );
                    let fault_addr = fault_addr_val as *mut c_void;
                    let mut mbi = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
                    let ok = unsafe {
                        Win32_Memory::VirtualQuery(
                            fault_addr,
                            &mut mbi,
                            core::mem::size_of::<Win32_Memory::MEMORY_BASIC_INFORMATION>(),
                        ) != 0
                    };
                    if ok {
                        eprintln!(
                            "[diag-unrecov-av-pagestate] addr={:p} BaseAddress={:p} RegionSize={:#x} State={:#x} Protect={:#x} Type={:#x} AllocationProtect={:#x}",
                            fault_addr,
                            mbi.BaseAddress,
                            mbi.RegionSize,
                            mbi.State,
                            mbi.Protect,
                            mbi.Type,
                            mbi.AllocationProtect,
                        );
                    } else {
                        eprintln!(
                            "[diag-unrecov-av-pagestate] addr={:p} VirtualQuery FAILED, GetLastError={}",
                            fault_addr,
                            unsafe { GetLastError() },
                        );
                    }
                }
                // Dump the top of this thread's real stack (module-relative RVAs where possible)
                // to recover the call chain even though `rip` itself is a wild jump into
                // non-code memory and cannot be symbolized or unwound normally.
                {
                    let rsp = context_snapshot.Rsp as usize;
                    for i in 0..32usize {
                        let addr = rsp.wrapping_add(i * 8);
                        // `read_unaligned`, not `read_volatile`: this code runs precisely when
                        // `rip` was a wild jump, and such a fault can leave `rsp` itself
                        // misaligned. `read_volatile` REQUIRES alignment, and a debug build's UB
                        // check turns that into a non-unwinding abort -- so the diagnostic meant
                        // to explain an unrecoverable AV was instead killing the process before
                        // printing anything, destroying the evidence it exists to capture.
                        // Confirmed live: "unsafe precondition(s) violated: ptr::read_volatile
                        // requires that the pointer argument is aligned", aborting mid-dump.
                        // `rsp` may not be a stack pointer at all at a wild-jump fault, so probe
                        // before dereferencing and stop at the first unreadable slot: the reason
                        // IS the diagnostic, and silently skipping would hide it.
                        let Some(val) = diag_probe_read_usize(addr) else {
                            eprintln!(
                                "[diag-unrecov-av-stack] [rsp+{:#x}] addr={addr:#x} NOT READABLE (rsp={rsp:#x} is not a usable stack pointer) -- stopping stack walk",
                                i * 8,
                            );
                            break;
                        };
                        let in_module = val.wrapping_sub(module_base) < 0x0200_0000;
                        eprintln!(
                            "[diag-unrecov-av-stack] [rsp+{:#x}]={:#x}{}",
                            i * 8,
                            val,
                            if in_module { " (in-module)" } else { "" },
                        );
                    }
                }
                // Print the ring of recent faults on THIS thread -- if this fault is a secondary
                // one (e.g. inside ntdll's own unwind machinery, reached only after an earlier,
                // more informative fault was already dispatched), the entries before the most
                // recent one recover what that earlier fault actually was.
                // `try_borrow` (not `with_borrow`): a nested/re-entrant fault reaching this print
                // while an outer invocation still holds `RECENT_FAULTS`/`RECOVERY_LOG`'s borrow
                // must never panic here -- that would re-enter this same exception path and has
                // been observed live to loop indefinitely instead of terminating, turning the
                // fault this code exists to diagnose into an unkillable hang. Skip printing this
                // ring on collision instead of panicking.
                RECENT_FAULTS.with(|cell| {
                    if let Ok(ring) = cell.try_borrow() {
                        for (i, (code, rip, fault_rsp, in_guest)) in ring.iter().enumerate() {
                            // `in_guest` is `Option<bool>`: `None` means this ring entry's thread
                            // had no TLS slot at capture time, i.e. thread identification could
                            // not be done -- print it as its own distinct state, never silently
                            // folded into "false", so a reader can tell "confirmed host thread"
                            // apart from "could not determine".
                            let in_guest_str = match in_guest {
                                Some(true) => "true",
                                Some(false) => "false",
                                None => "unknown(no-tls)",
                            };
                            eprintln!(
                                "[diag-unrecov-av-ring] [{i}] code={code:#x} rip={rip:#x} rva={:#x} rsp={fault_rsp:#x} is_in_guest={in_guest_str}",
                                (*rip as usize).wrapping_sub(module_base),
                            );
                            // Dump this ring entry's own top-of-stack too, so the caller of a
                            // fault like `write_u8_fallible` (a tiny leaf function with almost no
                            // prologue) can be recovered even when it isn't the LAST fault in the
                            // ring.
                            if *code == Win32_Foundation::EXCEPTION_ACCESS_VIOLATION
                                && *fault_rsp != 0
                            {
                                for j in 0..8usize {
                                    let addr = (*fault_rsp as usize).wrapping_add(j * 8);
                                    // See the alignment note on the primary stack dump above:
                                    // a recorded `fault_rsp` has the same misalignment hazard.
                                    // A recorded `fault_rsp` carries the identical garbage-value
                                    // hazard; guarding only the primary walk would still let this
                                    // one take out the handler.
                                    let Some(val) = diag_probe_read_usize(addr) else {
                                        eprintln!(
                                            "[diag-unrecov-av-ring-stack] [{i}][rsp+{:#x}] addr={addr:#x} NOT READABLE -- stopping",
                                            j * 8,
                                        );
                                        break;
                                    };
                                    let in_module = val.wrapping_sub(module_base) < 0x0200_0000;
                                    eprintln!(
                                        "[diag-unrecov-av-ring-stack] [{i}][rsp+{:#x}]={val:#x}{}",
                                        j * 8,
                                        if in_module { " (in-module)" } else { "" },
                                    );
                                }
                            }
                        }
                    }
                });
                RECOVERY_LOG.with(|cell| {
                    if let Ok(ring) = cell.try_borrow() {
                        for (i, (fault_rip, recover_addr)) in ring.iter().enumerate() {
                            if *fault_rip == 0 && *recover_addr == 0 {
                                continue;
                            }
                            eprintln!(
                                "[diag-unrecov-av-recovery] [{i}] fault_rip={fault_rip:#x} rva={:#x} -> recover={recover_addr:#x} rva={:#x}",
                                (*fault_rip as usize).wrapping_sub(module_base),
                                (*recover_addr as usize).wrapping_sub(module_base),
                            );
                        }
                    }
                });
                use std::io::Write;
                let _ = std::io::stderr().flush();
            }
            // `EXCEPTION_CONTINUE_SEARCH` is actively harmful here, for BOTH guest-mode and
            // host-mode unrecovered faults: `switch_to_guest`'s bare-`jmp` trampoline (and this
            // crate's other unwind-info-hostile hand-written-assembly call chains) leave no
            // legitimate frame for Windows' SEH unwinder to walk, so handing the fault onward
            // corrupts `ntdll` state rather than recovering. Terminate instead.
            // See docs/veh-exception-handler-design.md ("Unrecovered access violations now
            // terminate instead of EXCEPTION_CONTINUE_SEARCH") for the full evidence trail.
            diag_raw_print(
                b"[diag-unrecov-av-terminate] rip=0x",
                context_snapshot.Rip as usize,
                b" addr=0x",
                exception_record.ExceptionInformation[1],
            );
            // Arm `fault_terminate_watchdog_thread_body` (a genuinely different, always-standing-
            // by OS thread) BEFORE attempting self-termination below, in case that attempt does
            // not complete on its own -- see `FAULT_TERMINATE_ARMED_TICK`'s doc comment for the
            // full evidence this exists to cover. Any nonzero value arms it; a monotonic counter
            // (rather than a bare `1`) so a future diagnostic can distinguish which of possibly
            // several arm events the watchdog eventually acted on.
            FAULT_TERMINATE_ARMED_TICK.fetch_add(1, Ordering::SeqCst);
            // A self-`TerminateProcess` from inside the VEH was confirmed NOT to reliably
            // terminate the process on this host (the thread can be stuck behind the same
            // kernel-mode exception protocol it's trying to escape). `RaiseFailFastException`
            // uses a separate, always-fatal kernel path instead. See
            // docs/veh-exception-handler-design.md ("Why RaiseFailFastException, not a
            // self-TerminateProcess") for the live evidence.
            // SAFETY: passes real records for an exception already being handled by this VEH;
            // never returns.
            unsafe {
                windows_sys::Win32::System::Diagnostics::Debug::RaiseFailFastException(
                    exception_record as *const EXCEPTION_RECORD,
                    context as *const _
                        as *const windows_sys::Win32::System::Diagnostics::Debug::CONTEXT,
                    0,
                );
            }
            return EXCEPTION_CONTINUE_SEARCH;
        }
    }

    // Windows clears this thread's FS_BASE MSR back to 0 on its own initiative as part of
    // ordinary scheduling; an in-guest `mov %fs:...` then faults indistinguishably from a real
    // guest segfault. Detect and repair in place (no guest-mode exit) before any other
    // exception-code-specific handling. See docs/veh-exception-handler-design.md
    // ("FS_BASE-reset repair") for why this must happen here rather than via
    // `interrupt_callback`, and for the two guards below.
    if exception_record.ExceptionCode == Win32_Foundation::EXCEPTION_ACCESS_VIOLATION
        && unsafe { litebox_common_linux::rdfsbase() } == 0
        // `Rip == 0` => not a real instruction; don't loop re-faulting at address 0. Uses the
        // snapshot, not a live re-read, to avoid a torn read (mirrors the !is_in_guest branch).
        && context_snapshot.Rip != 0
        // Without this, a guest fault with no FS-segment prefix coinciding with
        // `rdfsbase() == 0` gets misdiagnosed as FS_BASE-reset and retried forever (confirmed
        // live, Node.js `process.title = <string>`). See the doc section above.
        && faulting_instruction_has_fs_override(context_snapshot.Rip.trunc())
    {
        let saved = WindowsUserland::get_thread_fs_base();
        if saved != 0 {
            if veh_trace_enabled() {
                eprintln!(
                    "[veh] tid={:?} FS_BASE-reset in-place repair (rip={:#x})",
                    std::thread::current().id(),
                    context_snapshot.Rip,
                );
            }
            // See the host-mode repair site above (AGENTS.md pass 302): a single write can lose
            // a race against another Windows-initiated FS_BASE reset under high-frequency
            // triggering, so verify and retry a bounded number of times before resuming.
            for _ in 0..8 {
                unsafe { litebox_common_linux::wrfsbase(saved) };
                if unsafe { litebox_common_linux::rdfsbase() } == saved {
                    break;
                }
            }
            return EXCEPTION_CONTINUE_EXECUTION;
        }
    }

    // A single-step trap while in guest mode belongs to the post-`fork()` verification machinery
    // (`EFLAGS.TF` is masked out of every guest-visible eflags value, so the guest can never arm
    // it itself). Either it is a clean step -- in which case we re-arm TF and resume without ever
    // leaving guest mode -- or it caught the child executing/writing through a stale pointer into
    // the parent's address space, in which case we fall through to the normal exception path with
    // a synthesized access violation so the child dies exactly as it would on real hardware.
    //
    // FS_BASE-reset repair applies here too, and matters far more: single-stepping makes every
    // guest instruction its own kernel round-trip through this handler, so a fork() child under
    // verification hits the reset on very nearly every instruction (>99% measured). Repairing
    // before `on_single_step` runs avoids a second access-violation-and-retry round trip per
    // step. See docs/veh-exception-handler-design.md ("Single-step path needs the same repair").
    if unsafe { litebox_common_linux::rdfsbase() } == 0 {
        let saved = WindowsUserland::get_thread_fs_base();
        if saved != 0 {
            unsafe { litebox_common_linux::wrfsbase(saved) };
        }
    }

    // A stale, untranslated source-range `rip` (fork_verify::on_single_step's case (1)) can
    // arrive as a raw EXECUTE access violation instead of EXCEPTION_SINGLE_STEP, depending on
    // incidental paging state -- confirmed live (litebox-xfce-1, dbus-daemon fork child). This
    // mirrors case (1)'s translate-and-resume fix; see docs/veh-exception-handler-design.md
    // ("A stale source-range rip can arrive as a raw AV").
    // The following lock must be a re-entrant BLOCKING acquisition, not a bounded-spin try_lock
    // (an earlier version's give-up path healed unserialized) -- held across both the AV-path
    // healers below and the on_single_step call further down so two threads' healing sequences
    // never interleave. See docs/veh-exception-handler-design.md ("The re-entrant heal lock").
    let _fork_verify_heal_guard = lock_fork_verify_heal_reentrant();

    if exception_record.ExceptionCode == Win32_Foundation::EXCEPTION_ACCESS_VIOLATION
        && fork_verify::is_verifying(tls)
    {
        // TORN-READ FIX (track-b sweep): `context_snapshot.Rip`, not a live re-read of
        // `context.Rip` -- this value drives the whole stale-pointer healing decision tree below
        // (`translate_stale_source_rip` and its three siblings), all real control flow. No write
        // to `context.Rip` occurs on this path before this point, so the snapshot and the live
        // value are still guaranteed identical here absent a torn read -- using the snapshot
        // removes the torn-read hazard with no behavior change on the non-torn path.
        #[allow(clippy::cast_possible_truncation)]
        let rip = context_snapshot.Rip as usize;
        // Livelock breaker: if this exact `(rip, translated_rip)` pair has already been "healed"
        // via the `[rsp-8]`/`[rsp]`/GPR fixups below many times in a row with no forward progress,
        // something else keeps re-supplying the identical stale value from a slot those fixups
        // don't reach -- skip straight to the deeper GOT/PLT-slot and register-indirect healers
        // instead of repeating the same ineffective fixup forever.
        const AV_RIP_LIVELOCK_THRESHOLD: u32 = 8;
        let prior_repeat = tls.fork_verify_av_rip_repeat.get();
        let skip_shallow_heal = matches!(
            prior_repeat,
            Some((prev_rip, _, count)) if prev_rip == rip && count >= AV_RIP_LIVELOCK_THRESHOLD
        );
        if !skip_shallow_heal
            && let Some(translated_rip) = fork_verify::translate_stale_source_rip(tls, rip, context)
        {
            let next_count = match prior_repeat {
                Some((prev_rip, prev_translated, count))
                    if prev_rip == rip && prev_translated == translated_rip =>
                {
                    count + 1
                }
                _ => 1,
            };
            tls.fork_verify_av_rip_repeat
                .set(Some((rip, translated_rip, next_count)));
            if veh_trace_enabled() {
                eprintln!(
                    "[veh] tid={:?} AV-path stale rip healed rip={rip:#x} translated={translated_rip:#x} repeat={next_count}",
                    std::thread::current().id(),
                );
            }
            // `fault_addr` is `ExceptionInformation[1]`, the address the access violation actually
            // faulted on. It is logged because without it this line reads as a confession: it is a
            // `warn!` naming a wild-looking address pair (a source-range `rip` in the 0x7fef_xxxx_xxxx
            // parent band translating to a child-band address orders of magnitude away), it is
            // frequently the last line before a fatal SIGSEGV, and it therefore looks exactly like a
            // mis-heal that jumped the guest into garbage. Twice now that reading has been wrong and
            // has cost a whole investigation: when `fault_addr != rip` the violation is a DATA access,
            // this healer's translation of `rip` is not what faulted and not what is about to fault
            // again, and the real cause is whatever register formed `fault_addr`. The live case that
            // forced this in was `advisor/ADVISORY-001-fundamentals.md` section 3N: `fault_addr` was
            // equal to `%rax`, the revealed-but-garbage safe-linked tcache `next` pointer, and the
            // identical instruction re-faulted on the identical `fault_addr` at `translated_rip`,
            // proving the translation byte-correct and this line innocent. One extra field settles
            // that at a glance instead of requiring a `LITEBOX_DIAG_FATALDUMP=1` register capture.
            litebox_util_log::warn!(
                rip:? = rip,
                translated_rip:? = translated_rip,
                fault_addr:? = exception_record.ExceptionInformation[1],
                repeat:? = next_count;
                "fork_verify: stale CODE pointer detected via raw access violation (no #DB delivered), translating and resuming"
            );
            #[allow(clippy::cast_possible_truncation)]
            {
                context.Rip = translated_rip as u64;
            }
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        if skip_shallow_heal {
            litebox_util_log::warn!(
                rip:? = rip;
                "fork_verify: AV-path stale rip livelock detected (same rip repeated), falling through to deeper slot healers"
            );
        }
        // `rip` itself was not stale -- the data-pointer counterpart to the code-pointer case
        // just above. A raw AV whose fault address is explained by a stale base/index register
        // in the faulting instruction's own memory operand (the register went stale earlier via
        // an ordinary register-to-register `mov` this module has no general single-step case
        // for) never reaches `on_single_step`'s case (2)/(2b) at all when it arrives as a raw AV
        // rather than a clean `#DB` -- the identical AV-bypass problem the `rip` healing above
        // exists for, just for a DATA pointer instead of a CODE pointer. Confirmed live
        // (litebox-xfce-1, dbus-daemon fork-child investigation): a thread's `rbp` held a stale
        // source-range value across a full single-step trap with no intervening healing
        // opportunity (case (1) only fires when `rip` itself lands in-source; this `rbp` never
        // did), then the next instruction dereferenced `[rbp+disp]` and faulted with the fault
        // address exactly equal to the stale `rbp` plus that displacement. Mirrors case (2)/(2b)
        // exactly (decode the instruction, translate its memory-operand registers through the
        // same relocation map, retry) via `translate_stale_source_memory_operand_registers`,
        // never advancing `rip` so the CPU re-executes the same instruction with the now-healed
        // register.
        if fork_verify::translate_stale_source_memory_operand_registers(tls, rip, context) {
            if veh_trace_enabled() {
                eprintln!(
                    "[veh] tid={:?} AV-path stale data-pointer register healed at rip={rip:#x}",
                    std::thread::current().id(),
                );
            }
            litebox_util_log::warn!(
                rip:? = rip;
                "fork_verify: stale DATA pointer register detected via raw access violation (no #DB delivered), translating and retrying"
            );
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        // The GOT/PLT-style counterpart to the two cases above: `rip` itself is not stale (already
        // ruled out above) and no memory-operand base/index register is stale either -- but `rip`
        // may be an indirect `call [mem]`/`jmp [mem]` whose explicit memory operand slot itself
        // holds a stale, untranslated CODE pointer (case (3) in `on_single_step`, exposed here for
        // the identical AV-bypass reason the two cases above already are). Confirmed live: this
        // exact gap is what let the SAME stale `rip`/`translated_rip` pair repeat unbounded even
        // after the `[rsp-8]` `ret`-target healing above landed -- a GOT/PLT slot has no fixed
        // offset from any register already in hand, so `[rsp-8]` never matched it, and the same
        // instruction's own read re-faulted on the identical stale slot forever. Healing the slot
        // in place here (not advancing `rip`) makes the CPU re-fetch through the now-correct
        // pointer on retry, and every subsequent call through the same PLT-style slot reads the
        // already-healed value directly.
        if fork_verify::translate_stale_source_indirect_call_target(tls, rip, context) {
            if veh_trace_enabled() {
                eprintln!(
                    "[veh] tid={:?} AV-path stale indirect call/jmp target slot healed at rip={rip:#x}",
                    std::thread::current().id(),
                );
            }
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        // The register-indirect counterpart to the slot-based case just above: `rip` is a
        // `call reg`/`jmp reg` whose target register was itself loaded from a stale slot one or
        // more instructions earlier (no memory operand on THIS instruction for case (3) above to
        // trace back to). Confirmed live as the actual remaining gap behind the identical
        // `rip=0x9d90733`/`translated_rip=0xa800733` pair recurring across a driving `sh`,
        // `xfwm4`, and `weston-desktop-shell` itself, even after every AV-path case above landed.
        if fork_verify::translate_stale_source_register_indirect_call_target(tls, rip, context) {
            if veh_trace_enabled() {
                eprintln!(
                    "[veh] tid={:?} AV-path stale register-indirect call/jmp target slot healed at rip={rip:#x}",
                    std::thread::current().id(),
                );
            }
            return EXCEPTION_CONTINUE_EXECUTION;
        }
    }

    let mut synthesized_record = None;
    if exception_record.ExceptionCode == Win32_Foundation::EXCEPTION_SINGLE_STEP {
        match fork_verify::on_single_step(tls, context) {
            fork_verify::StepOutcome::Continue => {
                if veh_gates().alloc_vec {
                    #[allow(clippy::cast_possible_truncation)]
                    let tf_armed = usize::from(context.EFlags & fork_verify::EFLAGS_TF as u32 != 0);
                    diag_raw_print(
                        b"[diag_tf] tf_armed=0x",
                        tf_armed,
                        b" steps=0x",
                        tls.fork_verify_step_count.get() as usize,
                    );
                    #[allow(clippy::cast_possible_truncation)]
                    let resume_rip = context.Rip as usize;
                    let mut buf = [0u8; 16];
                    let n = fork_verify::read_code_bytes_for_diagnostics(resume_rip, &mut buf);
                    let mut b0 = 0usize;
                    let mut b1 = 0usize;
                    for (i, byte) in buf[..n].iter().enumerate() {
                        if i < 8 {
                            b0 |= (*byte as usize) << (i * 8);
                        } else {
                            b1 |= (*byte as usize) << ((i - 8) * 8);
                        }
                    }
                    diag_raw_print(b"[diag_tf]   resume_rip=0x", resume_rip, b" bytes_lo=0x", b0);
                    diag_raw_print(b"[diag_tf]   bytes_hi=0x", b1, b" eflags=0x", context.EFlags as usize);
                }
                return EXCEPTION_CONTINUE_EXECUTION;
            }
            fork_verify::StepOutcome::StalePointer { address, is_write } => {
                if veh_gates().alloc_vec {
                    diag_raw_print(
                        b"[diag_stale_ptr] is_write=0x",
                        usize::from(is_write),
                        b" address=0x",
                        address,
                    );
                }
                // Report it as a page fault on the offending address so the shim raises the same
                // `SIGSEGV` on the child that real hardware would have raised.
                synthesized_record = Some(fork_verify::access_violation_record(
                    exception_record,
                    address,
                    is_write,
                ));
            }
        }
    }
    let exception_record: &EXCEPTION_RECORD =
        synthesized_record.as_ref().unwrap_or(exception_record);

    tls.is_in_guest.set(false);
    // Diagnostic-only (`LITEBOX_CTXWATCH=1`): the watchpoint's job for this guest-entry cycle is
    // done once we're leaving guest mode again; the next `ContinueOperation::Resume` re-arms it
    // fresh for the new `ctx`. Cheap no-op when never armed.
    ctxwatch::disarm();
    // Pass-30 (`LITEBOX_DIAG_WAIT4GATE=1`): if the `[diag-rip0]` block above (this same VEH call)
    // just recorded a pending guest-stack-slot address from a `rip=0` fault, arm the reactive
    // write-watchpoint on it now -- strictly AFTER `ctxwatch::disarm()` above, so it survives past
    // this handler's return instead of being immediately cleared by it. Pass 28/29 established
    // this is a genuine guest-side null branch (the guest's own stack held a bad zero at
    // `rsp - 8`), so unlike every pass 20-25 watchpoint (armed on the HOST-side `ctx.rip` field,
    // which pass 28 retracted as never actually implicated), this watches the address proven to
    // hold the bad value. Left armed across the crash: the process survives (pass 28 item 3), and
    // watching stays live for whatever the guest shell runs next in the SAME session, so a SECOND
    // repro attempt in the same run can catch a hit even though the exact faulting address differs
    // run-to-run (this pass reactively re-derives it fresh from each run's own first crash instead
    // of guessing a fixed address up front).
    if let Some(addr) = diag_take_pending_watch_addr() {
        ctxwatch::arm_addr(addr);
        eprintln!(
            "[diag-rip0-watch] tid={:?} armed reactive write-watch on faulting_rsp-8={:#x} (post-crash, for next repro attempt in this session)",
            std::thread::current().id(),
            addr,
        );
    }

    // From here on, `context` is being redirected into `exception_callback` or
    // `interrupt_callback` (host code), and control never returns to `fork_verify::on_single_step`
    // to re-arm or clear `TF` again. If `TF` were left set (a `fork()` child under verification
    // hit a genuine exception -- our own synthesized one above, or an unrelated real one, e.g. a
    // guest access violation that happens to occur mid-verification), the CPU would single-step
    // through `exception_callback`'s/`interrupt_callback`'s own host instructions with
    // `is_in_guest` now `false`, which the `!is_in_guest` branch above does not handle for
    // `EXCEPTION_SINGLE_STEP` -- an unhandled `STATUS_SINGLE_STEP` (`0x80000004`) that kills the
    // whole host process instead of just this child. Clear it unconditionally on every path that
    // leaves guest mode here, not just the `StalePointer` one.
    #[allow(clippy::cast_possible_truncation)]
    let eflags_tf = fork_verify::EFLAGS_TF as u32;
    context.EFlags &= !eflags_tf;

    let regs = unsafe { &mut *tls.guest_context_top.get().wrapping_sub(1) };
    save_guest_context(regs, context);

    // Note: an `EXCEPTION_ACCESS_VIOLATION` caused by a cleared FS_BASE is already handled above,
    // before `is_in_guest` was cleared and before the single-step triage ran -- nothing between
    // there and here writes FS_BASE, so by construction every remaining exception here is a
    // genuine one and always goes to `exception_callback`.
    //
    // Write the exception record into scratch space BELOW `host_sp`, well clear of the
    // `thread_ctx` pointer that `run_thread_arch`'s prologue pushed at `[host_sp]`/
    // `[host_sp + 8]`. `exception_callback` (like `syscall_callback` and `interrupt_callback`)
    // expects `[rsp] == thread_ctx`, so `Rsp` must land exactly on `host_sp`, unmodified -- it
    // must NOT be repointed into the exception-record scratch area itself. Previously `Rsp` was
    // set to the (16-byte-realigned) exception-record address instead of `host_sp`, so
    // `exception_callback`'s `mov rcx, [rsp]` read raw bytes from within the just-written
    // `EXCEPTION_RECORD` (misinterpreted as `&mut ThreadContext`) rather than the real
    // `thread_ctx` pointer -- observed in practice as `ThreadContext` fields reading back as
    // null/garbage.
    // REENTRANCY: this write target must NOT be a single fixed address. `vectored_exception_
    // handler_entry`'s own scratch slot is already depth-aware (see `TlsState::veh_depth`'s doc
    // comment and `VEH_DEPTH_CAP`) precisely because a nested exception can fire on this SAME
    // thread before an outer invocation has finished using its own scratch space -- the identical
    // hazard applies here: if a nested exception reaches this write while an outer invocation's
    // `exception_record: &EXCEPTION_RECORD` reference (constructed from this SAME fixed address,
    // just below) is still live, the nested write corrupts memory the outer call is still reading
    // through, a real data race even though both invocations run on one thread (one is suspended
    // partway through using the value when the other, nested, one runs). Confirmed live: before
    // this fix, a `LITEBOX_PROCESS_FORK=1` repro showed the SAME faulting `rip`/address repeating
    // for 3000+ nested VEH entries with real stack usage growing on every retry -- consistent with
    // each nested dispatch corrupting the outer one's in-flight `EXCEPTION_RECORD`, producing a
    // new, different-looking-but-caused-by-the-same-bug fault each time it resumed. Give each
    // nesting level (`veh_depth`, already incremented by the trampoline before this Rust code
    // ever runs) its own slice of the SAME already-committed `EXCEPTION_RECORD_RESERVE` (64 KiB)
    // region below `host_sp`, sized so `VEH_DEPTH_CAP` levels' worth of `size_of::<EXCEPTION_
    // RECORD>()`-sized slots fit comfortably under `EXCEPTION_RECORD_RESERVE` with room to spare
    // (152 bytes * 512 = 77824... too large alone, so round each slot up to a fixed, generous,
    // cache-line-friendly 128 bytes and cap how many levels get their own real slot; levels past
    // that fall back to slot 0, matching this reserve's own pre-existing size headroom rather
    // than growing the reservation itself, since a fault that deep already indicates a separate,
    // still-open bug per `VEH_DEPTH_CAP`'s own doc comment, not a case worth optimizing for).
    // The slot stride MUST be at least `size_of::<EXCEPTION_RECORD>()`, or consecutive nesting
    // levels' records physically overlap and each nested dispatch corrupts the tail of the record
    // the still-live outer invocation is reading through -- the exact "give each level its own
    // slice" hazard this per-depth scheme exists to prevent, reintroduced by an under-sized
    // stride. It was previously a hardcoded 128 while `EXCEPTION_RECORD` is 152 bytes on x86_64
    // (4 + 4 + 8 + 8 + 4 + 4 pad + 15*8), so slot N+1 began 24 bytes INSIDE slot N. Confirmed
    // live: a `touch` repro faulted at `veh_depth=1` and the outer invocation then read a
    // `ExceptionCode` of `0x470041` -- not any Windows status code, but UTF-16LE `"AG"`, i.e. raw
    // string bytes written over the outer record by the nested one -- after which dispatch
    // followed a garbage `ExceptionAddress`/`Rip` (observed `rip=0x22`, `0x40`) into the wild-jump
    // cascade that ends at the `[diag-unrecov-av-giveup]` circuit breaker. Derive the stride from
    // the type instead of restating it, rounded up to 16 so every slot stays 16-byte aligned, so
    // this can never silently drift out of sync with the struct again.
    const EXC_RECORD_SLOT_SIZE: usize = size_of::<EXCEPTION_RECORD>().next_multiple_of(16);
    // One slot per nesting level that can actually exist, plus one -- not "half the reserve".
    //
    // This was `EXCEPTION_RECORD_RESERVE / EXC_RECORD_SLOT_SIZE / 2`, i.e. 204 slots, against a
    // `VEH_DEPTH_CAP` of 7: every slot past the eighth was unreachable, and the 32 KiB they
    // occupied came straight out of the budget the assert below shares with the trampoline's
    // per-depth handler frames -- which is what forced those frames to be too small to hold the
    // handler and its callees. Sizing this to the levels that exist frees that space for the
    // frames, which is where it was needed.
    const EXC_RECORD_SLOTS: usize = VEH_DEPTH_CAP as usize + 1;
    // The slot region grows UP from `host_sp - EXCEPTION_RECORD_RESERVE`; the trampoline's own
    // per-depth scratch frames grow DOWN from `host_sp - 64` to `host_sp - 64 - VEH_DEPTH_CAP*64`.
    // These two regions must not meet. Enforced here rather than left to the prose that previously
    // asserted (incorrectly, by 64 bytes at the deepest level) that they were "comfortably clear".
    const _: () = assert!(
        EXC_RECORD_SLOT_SIZE >= size_of::<EXCEPTION_RECORD>(),
        "exception-record slot stride must not be smaller than the record it holds"
    );
    const _: () = assert!(
        EXCEPTION_RECORD_RESERVE - (EXC_RECORD_SLOTS * EXC_RECORD_SLOT_SIZE)
            >= (VEH_DEPTH_CAP as usize + 1) * VEH_FRAME_STRIDE as usize,
        "exception-record slots and the trampoline's per-depth handler frames must not overlap"
    );
    let depth = tls.veh_depth.get() as usize;
    let slot = depth.min(EXC_RECORD_SLOTS.saturating_sub(1));
    let exception_record_ptr = tls
        .host_sp
        .get()
        .cast::<u8>()
        .wrapping_byte_sub(EXCEPTION_RECORD_RESERVE)
        .wrapping_byte_add(slot * EXC_RECORD_SLOT_SIZE)
        .cast::<EXCEPTION_RECORD>();
    assert!(exception_record_ptr.is_aligned());
    // Explicitly `VirtualAlloc(MEM_COMMIT)` the target page before writing, rather than relying on
    // Windows' automatic guard-page stack growth: this single `write()` lands up to
    // `EXCEPTION_RECORD_RESERVE` (64 KiB) below `host_sp`, far past the thread's actual committed
    // stack depth in every captured crash this bug has ever produced (`committed=0x4000`, 16 KiB,
    // is the consistent real-world figure across dozens of `DIAG-REALSTACK` captures in this
    // investigation's history) -- a single write that far past the guard page does not reliably
    // trigger the normal one-page-at-a-time stack-growth fault Windows expects `__chkstk`-style
    // sequential probing to drive; it can instead raise `STATUS_STACK_OVERFLOW` on this exact
    // instruction, which VEH re-enters as a SECOND, nested exception on a thread already mid-
    // dispatch of the first one -- matching this investigation's own long-documented, previously
    // unexplained `rsp=0x1`/`code=0x1e` garbage second-exception signature exactly. This is
    // deliberately NOT another "probe more stack ahead of time" attempt (two prior variants of
    // that family were tried and refuted) -- it explicitly commits precisely the one page this
    // write needs, at the moment it needs it, via the OS's own allocation API rather than a
    // touch-and-hope memory access.
    unsafe {
        let commit_page = exception_record_ptr
            .cast::<u8>()
            .map_addr(|addr| addr & !0xFFF);
        let _ = windows_sys::Win32::System::Memory::VirtualAlloc(
            commit_page.cast(),
            4096,
            windows_sys::Win32::System::Memory::MEM_COMMIT,
            windows_sys::Win32::System::Memory::PAGE_READWRITE,
        );
    }
    unsafe { exception_record_ptr.write(*exception_record) };

    // Ensure that `run_thread_arch` is linked in so that `exception_callback` is visible.
    let _ = run_thread_arch as *const () as usize;

    // Update the thread context to jump to the exception handler.
    context.Rip = exception_callback as *const () as usize as u64;
    context.Rsp = tls.host_sp.get() as u64;
    context.Rbp = tls.host_bp.get() as u64;
    context.Rdx = exception_record_ptr as u64;

    EXCEPTION_CONTINUE_EXECUTION
}

fn save_guest_context(
    guest_context: &mut litebox_common_linux::PtRegs,
    context: &windows_sys::Win32::System::Diagnostics::Debug::CONTEXT,
) {
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
    } = guest_context;
    *r15 = context.R15.trunc();
    *r14 = context.R14.trunc();
    *r13 = context.R13.trunc();
    *r12 = context.R12.trunc();
    *rbp = context.Rbp.trunc();
    *rbx = context.Rbx.trunc();
    *r11 = context.R11.trunc();
    *r10 = context.R10.trunc();
    *r9 = context.R9.trunc();
    *r8 = context.R8.trunc();
    *rax = context.Rax.trunc();
    *rcx = context.Rcx.trunc();
    *rdx = context.Rdx.trunc();
    *rsi = context.Rsi.trunc();
    *rdi = context.Rdi.trunc();
    *orig_rax = context.Rax.trunc();
    *rip = context.Rip.trunc();
    // `EFLAGS.TF` is owned exclusively by the post-`fork()` verification machinery
    // (`fork_verify`), which arms it on guest entry and re-arms it on every trap. It must never
    // leak into guest-visible state: if it did, it would be restored on the next guest entry
    // (via `pushfq`/`popfq` in the syscall path, or `EFlags` in `switch_to_guest_ntcontinue`)
    // long after verification ended, producing single-step traps with nothing left to handle
    // them.
    *eflags = context.EFlags as usize & !fork_verify::EFLAGS_TF;
    *rsp = context.Rsp.trunc();
}

impl WindowsUserland {
    /// Create a new userland-Windows platform for use in `LiteBox`.
    ///
    /// # Panics
    ///
    /// Panics if the TLS slot cannot be created.
    pub fn new() -> &'static Self {
        // Resolve every exception-handler-reachable diagnostic gate now, while this is still
        // ordinary startup code. See [`VehGates`] for why doing it later -- lazily, from inside
        // the handler -- is not merely slower but unsafe.
        let _ = veh_gates();

        let mut sys_info = Win32_SysInfo::SYSTEM_INFO::default();
        Self::get_system_information(&mut sys_info);

        // TODO(chuqi): Currently we just print system information for
        // `TASK_ADDR_MIN` and `TASK_ADDR_MAX`.
        // Will remove these prints once we have a better way to replace
        // the current `const` values in PageManagementProvider.
        #[cfg(debug_assertions)]
        {
            println!("System information.");
            println!(
                "=> Max user address: {:#x}",
                sys_info.lpMaximumApplicationAddress as usize
            );
            println!(
                "=> Min user address: {:#x}",
                sys_info.lpMinimumApplicationAddress as usize
            );
        }

        let reserved_pages = Self::read_memory_maps::<4096>();

        // Pre-reserve the well-known low address band where common Alpine/musl/Debian `ET_EXEC`
        // (non-PIE) binaries conventionally load (`gcc`'s own fixed base is `0x400000`; confirmed
        // live via `DIAG sys_mmap: entry` evidence that its colliding segment needs up to roughly
        // `0x618000`) -- BEFORE any guest OS thread gets spawned, so Windows' own default
        // thread-stack-placement algorithm is forced to choose a different, non-colliding address
        // for every guest thread stack from the very first one. Deliberately reserved AFTER
        // `read_memory_maps` above, not before: `reserved_pages` feeds
        // `PageManagementProvider::reserved_pages`, which litebox's OWN guest-address allocator
        // treats as off-limits for guest allocations -- this reservation must stay invisible to
        // that check, since a genuine `ET_EXEC` binary still needs to load at this EXACT address
        // (real Linux gives it no other choice; `MAP_FIXED` `Replace`-mode already knows how to
        // decommit-and-recommit over a plain `MEM_RESERVE`, see `allocate_pages`). This call only
        // ever needs to keep a REAL OS thread's stack from landing here first; it is never
        // committed, and a subsequent guest `MAP_FIXED` `Replace`-mode `mmap` simply reclaims it
        // like any other free-but-reserved region.
        //
        // Widened from `0x600000` to `0x1000000` (6MiB -> 16MiB band, `0x400000..0x1400000`,
        // 2026-09-16): the original 6MiB size was tuned only to `gcc`'s own ~`0x618000` need and
        // left a real, live-confirmed gap for anything bigger. `readelf -l` on the guest's real
        // colliding binary (Debian 13's stock `python3.13` 3.13.5-2+deb13u4, selkies' own shebang
        // interpreter, entry `0x67b0d0`) shows its `PT_LOAD` segments span `0x400000` (first LOAD)
        // through its RW/BSS segment's real end, `0x9eedb8 + MemSiz(0x104f90) = 0xaf3d48`
        // (page-rounded `0xaf4000`) -- ~999KiB *above* the old `0xa00000` reservation ceiling, i.e.
        // completely unprotected. This is the `spawn_exec_collision_child`/
        // `AllocationError::AddressInUse` (`Errno(EEXIST)`) collision this project chased across
        // many sessions (`docs/AGENTS_ARCHIVE_2026-09-16.md`): a real Windows OS thread stack (or
        // other host allocation) is free to land in that unreserved `0xa00000..0xaf4000` gap, and
        // when it does, python3's own fixed-address `PT_LOAD` later collides with it. `0x1000000`
        // clears python3.13's real `0xaf4000` ceiling with ~5.5MiB of margin for other non-PIE
        // binaries this or a future image may exec at this same conventional base.
        unsafe {
            windows_sys::Win32::System::Memory::VirtualAlloc(
                0x0040_0000 as *const core::ffi::c_void,
                0x0100_0000,
                Win32_Memory::MEM_RESERVE,
                Win32_Memory::PAGE_NOACCESS,
            );
        }

        let platform = Self {
            reserved_pages,
            sys_info: std::sync::RwLock::new(sys_info),
            net_gateway: std::sync::OnceLock::new(),
            console_stdin_reader: std::sync::OnceLock::new(),
            cow_regions: std::sync::RwLock::new(std::collections::BTreeMap::new()),
        };

        // Start the NAT gateway eagerly IF a port is published (`LITEBOX_PUBLISH`). The gateway is
        // otherwise lazy, initialized by the guest's first outbound packet -- but a guest that only
        // `listen()`s never sends one, so a published port would never bind. See
        // `net::init_published_ports`.
        net::init_published_ports(&platform.net_gateway);

        // Initialize it's own fs-base (for the main thread)
        WindowsUserland::init_thread_fs_base();

        // Windows sets FS_BASE to 0 regularly upon scheduling; we register an exception handler
        // to set FS_BASE back to a "stored" value whenever we notice that it has become 0.
        //
        // Registered FIRST in the process's VEH chain (`1`), not last (`0`).
        //
        // The `0` here dated to the initial commit and was never a decision -- confirmed by
        // `git log -S`, which finds no change to this line since. It is the wrong value. This
        // handler owns this process's guest-execution fault semantics outright: FS_BASE repair,
        // the guest-address-stack swap, `fork_verify`'s single-step walk, and delivery of a real
        // guest `SIGSEGV`. Nothing else loaded into the process has any business seeing a guest
        // fault first, and anything that does can `EXCEPTION_CONTINUE_EXECUTION` straight out from
        // under this handler -- silently, with no trace -- which is exactly the possibility
        // AGENTS.md's `RtlpUnwindPrologue` section asks to rule out before spending further
        // investigation passes on faults that never reach here.
        //
        // Safe in the other direction too: this handler is already a good citizen for exceptions
        // it does not own, falling through to `EXCEPTION_CONTINUE_SEARCH` on every path it does
        // not deliberately handle (the naked entry point does so without entering Rust at all),
        // so going first cannot swallow a host-side exception that belongs to someone else.
        unsafe {
            let _ = AddVectoredExceptionHandler(1, Some(vectored_exception_handler_entry));
        }

        // Register a console control handler to receive Ctrl+C / Ctrl+Break
        // Diagnostic (temporary, this investigation pass): LITEBOX_DIAG_NO_CTRLC_HANDLER=1 skips
        // registration entirely, to test whether Ctrl+C/Ctrl+Break event delivery is ever a
        // factor in the still-open "silent host crash on the 3rd fork" bug.
        if std::env::var_os("LITEBOX_DIAG_NO_CTRLC_HANDLER").is_none() {
            unsafe {
                windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
                    Some(ctrl_c_handler),
                    1, // TRUE — add the handler
                );
            }
        }

        // Watch for real console window resizes and deliver SIGWINCH. There is no Win32 resize
        // *event* callback equivalent to `SetConsoleCtrlHandler` -- `GetConsoleScreenBufferInfo`
        // polling on a dedicated thread is the standard approach (e.g. used by libuv/Node's own
        // Windows tty backend). This deliberately does not touch `STD_INPUT_HANDLE` or the input
        // event queue at all (unlike `ConsoleStdinReader`), so it cannot race with or steal
        // events from the existing stdin reader thread -- `GetConsoleScreenBufferInfo` reads the
        // *output* buffer's window-size state, a wholly separate API surface.
        // Live cdb inspection of a real crash (`rsp` corrupted inside `ntdll!RtlDispatchException`,
        // the same signature already root-caused to `exception_table::write_u8_fallible` failing to
        // recover) showed OTHER real OS threads on this process whose call stacks read
        // `console_resize_watcher_thread_body` -> ... -> `syscall_callback` -> `pty_ioctl` --
        // i.e. a thread this closure spawned (with Rust's plain, unsized default stack, no
        // `.stack_size()` call, unlike every properly-sized guest thread) is later reused to
        // service a REAL guest syscall once its own polling loop returns/exits. Give it the same
        // `GUEST_THREAD_STACK_SIZE` headroom every other guest-work-capable thread gets, closing
        // that gap rather than leaving this one thread as the sole undersized exception.
        const GUEST_THREAD_STACK_SIZE: usize = 32 * 1024 * 1024;
        // Diagnostic (temporary, this investigation pass): LITEBOX_DIAG_NO_RESIZE_WATCHER=1 skips
        // spawning this thread entirely, to test whether it (or the SIGWINCH delivery it
        // performs) is ever a factor in the still-open "silent host crash on the 3rd fork" bug.
        if std::env::var_os("LITEBOX_DIAG_NO_RESIZE_WATCHER").is_none() {
            std::thread::Builder::new()
                .name("litebox-console-resize-watcher".to_owned())
                .stack_size(GUEST_THREAD_STACK_SIZE)
                .spawn(console_resize_watcher_thread_body)
                .expect("failed to spawn console resize watcher thread");
        }

        // Track-B investigation (fork-without-exec hang): see `FAULT_TERMINATE_ARMED_TICK`'s doc
        // comment for the full evidence. A thread already inside kernel-mode exception delivery
        // for its own fault cannot reliably terminate its own process from within
        // (`TerminateProcess`/`RaiseFailFastException` called from that exact thread were both
        // observed live to be issued -- confirmed via their own diagnostic prints appearing in
        // the log -- yet not to complete), so the actual, working termination must come from a
        // genuinely different thread, exactly like the external `Stop-Process -Force` this
        // session confirmed DOES work immediately every time. This watchdog is that different
        // thread: it sleeps in a loop and, once `vectored_exception_handler` arms
        // `FAULT_TERMINATE_ARMED_TICK` (set immediately before its own now-unreliable
        // self-termination attempt), gives the primary attempt a short bounded grace period to
        // succeed on its own, then force-terminates the whole process itself if it has not.
        // Skippable via `LITEBOX_DIAG_NO_FAULT_WATCHDOG=1` for a future investigation that needs
        // to observe a wedged process without this safety net collecting it.
        if std::env::var_os("LITEBOX_DIAG_NO_FAULT_WATCHDOG").is_none() {
            std::thread::Builder::new()
                .name("litebox-fault-terminate-watchdog".to_owned())
                .spawn(fault_terminate_watchdog_thread_body)
                .expect("failed to spawn fault-terminate watchdog thread");
        }

        Box::leak(Box::new(platform))
    }

    /// Register a CoW-eligible memory region backed by a file. Mirrors
    /// `litebox_platform_linux_userland::LinuxUserland::register_cow_region` exactly.
    ///
    /// # Panics
    ///
    /// Panics if an overlapping region is already registered.
    pub fn register_cow_region(&self, data: &'static [u8], file_path: impl Into<std::path::PathBuf>) {
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

    /// Look up the file backing a static slice for CoW mapping. Mirrors
    /// `litebox_platform_linux_userland::LinuxUserland::lookup_cow_region` exactly.
    ///
    /// Returns `Some((file_path, offset_in_file))` if the slice is backed by a registered
    /// CoW region, `None` otherwise.
    fn lookup_cow_region(&self, source_data: &'static [u8]) -> Option<(std::path::PathBuf, usize)> {
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

    /// Reinterprets `&self` as `&'static Self`.
    ///
    /// # Why this is sound
    ///
    /// [`Self::new`] always returns its result from `Box::leak`, and this crate creates exactly
    /// one `WindowsUserland` per process (there is no `Drop` impl, no way to reclaim the leaked
    /// allocation, and every entry point that could construct a second instance is either test-only
    /// or documented as such) -- so any `&self` reachable from an instance method is, in practice,
    /// already borrowed from that single `'static` allocation. This exists specifically for
    /// [`ConsoleStdinReader::get`], whose background reader thread must outlive the calling stack
    /// frame (see its doc comment); every other instance method continues to take a plain `&self`
    /// with its natural (shorter, borrow-checked) lifetime, so this cast is used only where a
    /// `'static` bound is genuinely required, not as a blanket escape hatch.
    fn as_static(&self) -> &'static Self {
        // Safety: see the doc comment above -- `self` is always ultimately derived from a
        // `Box::leak`'d allocation with no legitimate way to outlive the process.
        unsafe { &*core::ptr::from_ref(self) }
    }

    fn read_memory_maps<const ALIGN: usize>() -> alloc::vec::Vec<core::ops::Range<usize>> {
        let mut reserved_pages = alloc::vec::Vec::new();
        let mut address = 0usize;
        // Only regions inside the guest's own addressable range are ever meaningful to
        // `Vmem`'s placement logic (see `TASK_ADDR_MAX`'s and `HOST_ALLOCATOR_REGION_MIN`'s doc
        // comments): anything at or above `TASK_ADDR_MAX` belongs to the host process itself
        // (its own stack, loaded modules, TEB, and -- since this split was introduced -- the
        // host global allocator's own reserved region) and must never be recorded as "reserved
        // guest space". Recording it anyway would make `Vmem::new_excluding`'s
        // `last_range_value()` see a spurious high-address entry, defeating the top-down
        // placement fast path and forcing every guest allocation through the slower gap-search
        // fallback -- or exhausting it outright once `TASK_ADDR_MAX` sits meaningfully below the
        // real top of the process address space (confirmed live: this broke ordinary anonymous
        // `mmap`/`mremap` once `TASK_ADDR_MAX` was lowered by 64 GiB to make room for
        // `HOST_ALLOCATOR_REGION_MIN`).
        let task_addr_max =
            <WindowsUserland as litebox::platform::PageManagementProvider<ALIGN>>::TASK_ADDR_MAX;

        loop {
            if address >= task_addr_max {
                break;
            }
            let mut mbi = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
            let ok = unsafe {
                Win32_Memory::VirtualQuery(
                    address as *const c_void,
                    &raw mut mbi,
                    core::mem::size_of::<Win32_Memory::MEMORY_BASIC_INFORMATION>(),
                ) != 0
            };
            if !ok {
                break;
            }

            if mbi.State == Win32_Memory::MEM_RESERVE || mbi.State == Win32_Memory::MEM_COMMIT {
                let start = mbi.BaseAddress as usize;
                let end = (start + mbi.RegionSize).min(task_addr_max);
                if start < end {
                    reserved_pages.push(core::ops::Range { start, end });
                }
            }

            address = mbi.BaseAddress as usize + mbi.RegionSize;
            if address == 0 {
                break;
            }
        }

        reserved_pages
    }

    /// Retrieves information about the host platform (Windows).
    fn get_system_information(sys_info: &mut Win32_SysInfo::SYSTEM_INFO) {
        unsafe {
            Win32_SysInfo::GetSystemInfo(sys_info);
        }
    }

    fn round_up_to_granu(&self, x: usize) -> usize {
        let gran = self.sys_info.read().unwrap().dwAllocationGranularity as usize;
        (x + gran - 1) & !(gran - 1)
    }

    fn round_down_to_granu(&self, x: usize) -> usize {
        let gran = self.sys_info.read().unwrap().dwAllocationGranularity as usize;
        x & !(gran - 1)
    }

    pub fn init_task(&self) -> litebox_common_linux::TaskParams {
        // Credentials are root (uid/gid 0), matching a real container's initial process (a
        // fresh OCI/container rootfs such as Alpine ships `/`, `/etc`, `/lib`, etc. root-owned
        // at mode 0755, and its init process runs as root absent an explicit `USER` directive).
        // Callers that build the guest's file system (e.g.
        // `litebox_runner_linux_on_windows_userland`) must set the in-memory file system's
        // persistent user to match via `litebox::fs::in_mem::FileSystem::set_default_user`, or
        // `getuid()` will disagree with what the filesystem layer's permission checks enforce.
        litebox_common_linux::TaskParams {
            pid: 1,
            ppid: 0,
            uid: 0,
            gid: 0,
            euid: 0,
            egid: 0,
        }
    }
}

impl litebox::platform::Provider for WindowsUserland {}

impl litebox::platform::SignalProvider for WindowsUserland {
    type Signal = litebox_common_linux::signal::Signal;

    fn take_pending_signals(&self, mut f: impl FnMut(Self::Signal)) {
        let bits = get_tls_ptr().map_or(0, |p| {
            unsafe { &*p }
                .pending_host_signals
                .swap(0, Ordering::SeqCst)
        });
        let sigs = litebox_common_linux::signal::SigSet::from_u64(u64::from(bits));
        for signal in sigs {
            f(signal);
        }
    }
}

/// Ensures the module-wide TLS slot index ([`TLS_INDEX`]) has been allocated.
///
/// This must be called before any code that reads `TLS_INDEX`. Both
/// [`run_thread`] (guest threads) and `WindowsUserland`'s `ThreadProvider::run_test_thread`
/// (test threads, only present in `#[cfg(debug_assertions)]` builds) go through here.
fn ensure_tls_index() {
    // Allocate a TLS slot for this module if not already done. This is used as
    // a place to store data across calls to the guest, since all the registers
    // are used by the guest and will be clobbered.
    //
    // We use this instead of native TLS because accesses are easier from
    // assembly. In particular, finding the module's TLS base requires extra
    // registers and/or clobbering flags, whereas we can get the value of a
    // TLS slot with only one register and no changes to flags.
    static REGISTER_KEY: std::sync::Once = const { std::sync::Once::new() };
    REGISTER_KEY.call_once(|| {
        let index = unsafe { windows_sys::Win32::System::Threading::TlsAlloc() };
        assert!(
            index < 64,
            "no non-extended TLS slots available: {index:#x}"
        );
        TLS_INDEX.store(index, Ordering::Relaxed);
    });
}

/// Sets this (the CALLING, current) OS thread's own [`CURRENT_GUEST_PID`] directly. Unlike
/// [`ThreadProvider::set_next_spawned_thread_guest_pid`] (a `thread_local!` read by the SPAWNED
/// thread's own closure, captured by value across the thread hop -- see that function's and
/// [`CURRENT_GUEST_PID`]'s doc comments for why a `thread_local!` set on the spawning thread is
/// invisible to a plain `.take()` on the newly spawned thread, since each thread's own
/// `thread_local!` storage starts fresh), this must be called FROM the thread whose identity is
/// being set -- there is no cross-thread propagation here, just a direct same-thread set. Exists
/// for the ROOT/initial guest process specifically: its OS thread is spawned directly by the
/// runner crate via a plain `std::thread::Builder` call, entirely outside `spawn_thread`'s own
/// propagation machinery, so without an explicit same-thread call like this one, that root
/// process's `CURRENT_GUEST_PID` stayed `None` for its whole lifetime -- confirmed live via a real
/// `gcc`-under-litebox crash: the root process's own genuinely-live memory claims, tagged under
/// `allocate_pages`'s raw-`ThreadId` `ClaimOwner` fallback instead of `GuestPid`, were misidentified
/// as belonging to a foreign process by an unrelated later guest process's own fixed-address
/// allocation, forcing an unnecessary relocation that broke that later process's `ET_EXEC` binary
/// load. Re-landed a second time after an earlier revert (`d5e2556f`) proved a net-negative
/// regression WITHOUT this session's own later `LIVE_THREAD_STACKS` fix (`e1978f2c`) in place --
/// that fix specifically defends against a live thread's real stack being wrongly overwritten by a
/// colliding `MAP_FIXED` load, which is exactly the collision class this identity fix's own
/// regression exposed (a formerly-masked `sh`/`weston` vs a later child collision). With both
/// fixes together, re-verify carefully (the same live A/B methodology as the first attempt) before
/// trusting this landed correctly.
pub fn set_current_thread_guest_pid(pid: i32) {
    CURRENT_GUEST_PID.set(Some(pid));
}

/// Runs a guest thread using the provided shim and the given initial context.
///
/// This will run until the thread terminates.
///
/// # Safety
/// The context must be valid guest context.
pub unsafe fn run_thread(
    shim: impl litebox::shim::EnterShim<ExecutionContext = litebox_common_linux::PtRegs>,
    ctx: &mut litebox_common_linux::PtRegs,
) {
    ensure_tls_index();
    run_thread_inner(&shim, ctx, None);
}

/// Identical to [`run_thread`], but additionally arms [`fork_verify`]'s post-fork stale-pointer
/// single-step verification for this thread -- the cross-process fork() child's equivalent of what
/// the thread-based fork path's own `ThreadInitState::ForkedChild` dispatch achieves via
/// `EnterShim::init` (`litebox_shim_linux/src/syscalls/process.rs`'s `begin_fork_child_verification`
/// call, itself invoked from inside `run_thread_arch`'s guest-entry machinery, strictly after TLS
/// installation).
///
/// # Why this exists (pass 143)
///
/// A cross-process fork() child is built via `LinuxShim::adopt_forked_process`, not `do_clone`'s
/// same-process `ThreadInitState::ForkedChild` path, so it never goes through that dispatch. Before
/// this function existed, callers (the diagnostic/production task-resume probes in
/// `litebox_runner_linux_on_windows_userland`) called
/// `ForkChildVerificationProvider::begin_fork_child_verification` directly, BEFORE calling
/// [`run_thread`] -- but that provider method's own implementation (`fork_verify::begin`) only takes
/// effect if `get_tls_ptr()` returns `Some`, which is only true from partway through
/// [`run_thread`]'s own internals ([`ThreadHandle::run_with_handle`]'s `install_tls` call) onward.
/// Called too early, `fork_verify::begin`'s `tls.fork_verify = Some(relocations)` step was silently
/// a no-op (the `if let Some(tls) = get_tls_ptr()` guard simply never entered its body), leaving
/// `fork_verify::is_verifying` permanently `false` for the whole resumed thread -- the guest's
/// single-step healing regime never engaged, and any stale pointer left over from the parent's
/// `WriteProcessMemory` memory copy went completely unrepaired, producing an unexplained
/// `STATUS_ACCESS_VIOLATION` on the guest's very first few instructions with zero VEH trace output
/// (the SAME symptom shape as a missing FS base, but a distinct root cause).
///
/// # Safety
/// Same contract as [`run_thread`].
pub unsafe fn run_thread_with_fork_verification(
    shim: impl litebox::shim::EnterShim<ExecutionContext = litebox_common_linux::PtRegs>,
    ctx: &mut litebox_common_linux::PtRegs,
    relocations: Arc<litebox::mm::AddressRelocations>,
) {
    ensure_tls_index();
    run_thread_inner(&shim, ctx, Some(relocations));
}

fn run_thread_inner(
    shim: &dyn litebox::shim::EnterShim<ExecutionContext = litebox_common_linux::PtRegs>,
    ctx: &mut litebox_common_linux::PtRegs,
    fork_verify_relocations: Option<Arc<litebox::mm::AddressRelocations>>,
) {
    let tls_state = TlsState::new();
    tls_state
        .guest_context_top
        .set(std::ptr::from_mut(ctx).wrapping_add(1));

    // Diagnostic only (LITEBOX_DIAG_TLS_ADDR): print this thread's own TlsState address to check
    // for cross-thread TlsState address collisions -- see AGENTS.md's "DEFINITIVE (4th pass)"
    // entry, which found the earlier watchpoint captures never actually observed a second thread.
    if std::env::var_os("LITEBOX_DIAG_TLS_ADDR").is_some() {
        eprintln!(
            "[diag-tls-addr] pid={} tid={:?} tls_state={:p}",
            std::process::id(),
            std::thread::current().id(),
            &tls_state,
        );
        use std::io::Write;
        let _ = std::io::stderr().flush();
    }

    let mut thread_ctx = ThreadContext {
        shim,
        ctx,
        tls: &tls_state,
    };
    ThreadHandle::run_with_handle(&tls_state, || unsafe {
        // Arm fork_verify (if requested) strictly AFTER `run_with_handle`'s own `install_tls` call
        // (already done by the time this closure body runs) and strictly BEFORE `run_thread_arch`
        // ever resumes guest code -- see this function's caller, `run_thread_with_fork_verification`,
        // for why the timing matters.
        if let Some(relocations) = fork_verify_relocations {
            fork_verify::begin(relocations);
        }
        run_thread_arch(&mut thread_ctx, &tls_state);
    });
}

static TLS_INDEX: AtomicU32 = AtomicU32::new(u32::MAX);

struct TlsState {
    host_sp: Cell<*mut u128>,
    host_bp: Cell<*mut u128>,
    guest_context_top: Cell<*mut litebox_common_linux::PtRegs>,
    /// The guest's `xmm0`-`xmm5` (the Windows x64 ABI's *caller-saved* SSE registers) at the
    /// moment the guest last entered the host via `syscall_callback`. These are deliberately not
    /// part of `PtRegs` (a Linux ABI-shaped struct whose layout other code depends on via fixed
    /// byte offsets, e.g. `switch_to_guest_sysret`'s hardcoded field offsets) -- this is
    /// host-only bookkeeping.
    ///
    /// `run_thread_arch`'s prologue already saves/restores `xmm6`-`xmm15` (the ABI's
    /// *callee*-saved SSE registers) around the entire guest-thread lifetime, because the host
    /// Rust code that runs between guest entries is a normal Windows x64 callee and is only
    /// obligated to preserve those. `xmm0`-`xmm5` are caller-saved, so any host code path
    /// reached from `syscall_callback` (the syscall handler, allocator code it calls into, etc.)
    /// is free to clobber them -- and every guest resume path (`switch_to_guest_sysret`,
    /// `switch_to_guest_ntcontinue`/`NtContinue`) previously left whatever value was physically
    /// in those registers untouched, silently replacing the guest's own `xmm0`-`xmm5` with
    /// leftover host state on every single syscall return. `NtContinue`'s `CONTEXT` was also
    /// never given `CONTEXT_FLOATING_POINT`, so the slow resume path had the identical gap.
    guest_xmm0_5: Cell<[u128; 6]>,
    /// Mirrors `THREAD_FS_BASE`'s value for this thread, kept in sync by
    /// `WindowsUserland::set_thread_fs_base`. `THREAD_FS_BASE` itself is a Rust `thread_local!`,
    /// whose storage location is not reachable from hand-written assembly the way a plain
    /// `#[repr(Rust)]` struct field is via `core::mem::offset_of!` -- this mirror exists solely so
    /// `vectored_exception_handler_entry`'s naked fast path can read the saved FS base value
    /// without depending on `thread_local!`'s internal ABI.
    guest_fs_base: Cell<usize>,
    scratch: Cell<usize>,
    /// Diagnostic-only (`LITEBOX_DIAG_ALLOC_VEC=1`): counts how many nested
    /// `vectored_exception_handler_entry` invocations are currently live on this thread, to
    /// directly witness whether the trampoline's fixed `host_sp - 48` scratch window (see that
    /// function's doc comment) is ever reentered while a prior invocation is still using it.
    veh_depth: Cell<u32>,
    is_in_guest: Cell<bool>,
    interrupt: Cell<bool>,
    continue_context:
        Box<std::cell::UnsafeCell<windows_sys::Win32::System::Diagnostics::Debug::CONTEXT>>,
    /// Bitmask of pending host-originated signals for this thread.
    pending_host_signals: AtomicU32,
    /// Pointer to the `Waker` currently being waited on, or null if not
    /// waiting.
    waiting_waker: std::sync::atomic::AtomicPtr<litebox::event::wait::Waker<WindowsUserland>>,
    /// Whether this host thread has ever entered guest mode before. `switch_to_guest`'s
    /// `rcx == rip` fast path (`switch_to_guest_sysret`) relies on genuine `sysret`-style CPU
    /// semantics that are only valid for a thread resuming guest mode after a PRIOR entry via
    /// the `syscall` instruction on this exact thread; a brand-new host thread's very first
    /// transition into guest mode (e.g. a `fork()`-created child resuming into a copy of the
    /// parent's syscall-entry context, where `rcx == rip` holds by coincidence) must always use
    /// the slower but universally-correct `NtContinue` path instead.
    has_entered_guest: Cell<bool>,
    /// The post-`fork()` address-space relocation map this thread's guest execution is being
    /// verified against, or `None` if this thread is not a `fork()` child under verification.
    ///
    /// See [`fork_verify`] and [`litebox::platform::ForkChildVerificationProvider`].
    fork_verify: RefCell<Option<Arc<litebox::mm::AddressRelocations>>>,
    /// Count of single-step traps [`fork_verify::on_single_step`] has processed on this thread
    /// since the most recent [`fork_verify::begin`], used only to bound verification duration for
    /// an IDENTITY (cross-process) relocation map -- see `MAX_IDENTITY_VERIFICATION_STEPS`'s doc
    /// comment. Meaningless (and never consulted) once `fork_verify` is `None`; reset to `0` by
    /// every [`fork_verify::begin`] call, matching that method's own reset of `fork_verify` itself.
    fork_verify_step_count: Cell<u64>,
    /// Tracks `(rip, translated_rip)` and a repeat count for the AV-path stale-CODE-pointer case
    /// (`translate_stale_source_rip`, see its call site in `vectored_exception_handler`) so a
    /// livelock where the SAME stale `rip` recurs unbounded after being "healed" every time (a
    /// persistent slot re-supplying the identical stale value on each loop iteration, not covered
    /// by that case's own `[rsp-8]`/`[rsp]`/GPR healing) can be detected and broken by falling
    /// through to the deeper memory-operand/indirect-slot/register-indirect healers even though
    /// `translate_stale_source_rip` itself keeps reporting success. Confirmed live: 266,408
    /// identical `(rip, translated_rip)` AV events in 8.4s during a real XFCE `--gui` launch.
    fork_verify_av_rip_repeat: Cell<Option<(usize, usize, u32)>>,
    /// The single-step-path counterpart to [`Self::fork_verify_av_rip_repeat`]: tracks
    /// `(rip, translated_rip)` and a repeat count for [`fork_verify::on_single_step`]'s own case
    /// (1) (the stale-CODE-pointer-in-`rip` heal reached via `EXCEPTION_SINGLE_STEP`, not the raw
    /// `EXCEPTION_ACCESS_VIOLATION` `translate_stale_source_rip` guards). Case (1) always
    /// translates `rip`/`rbp`/`rdi` and patches `[rsp-8]` when it is the just-popped `ret` target,
    /// but a guest loop whose stale value is re-supplied from a slot `[rsp-8]` does not reach (a
    /// GOT/PLT-style slot, or a register-indirect load chain) keeps re-arriving at the identical
    /// `(rip, translated_rip)` pair every iteration -- confirmed live: 357 consecutive identical
    /// heals of one pair in 216ms during one boot. Once this counter reaches the same
    /// `AV_RIP_LIVELOCK_THRESHOLD` used by the AV-path breaker, case (1) additionally falls
    /// through to the same deeper healers (`translate_stale_source_indirect_call_target`,
    /// `translate_stale_source_register_indirect_call_target`) that close this exact gap on the
    /// AV path, so the underlying slot is patched in place and the loop's later iterations no
    /// longer re-trap at all -- healed once, not on every pass.
    fork_verify_step_rip_repeat: Cell<Option<(usize, usize, u32)>>,
    /// The provenance chain [`fork_verify::on_single_step`] is tracking for the most recent
    /// explicit-memory-operand read on this thread, or `None` if no register currently carries a
    /// value traceable back to a specific memory slot this way.
    ///
    /// Lets [`fork_verify::on_single_step`] recognize a register-indirect `call reg`/`jmp reg`
    /// (or a case (2c) memory read) whose target was loaded from a stale memory slot -- possibly
    /// several instructions earlier, and possibly after the loaded value was advanced by simple
    /// constant-offset pointer arithmetic (`add`/`sub`/`lea` naming only the tracked register and
    /// an immediate, e.g. `mov reg, [slot]` then `add reg, 8` then `call reg`) -- so that slot can
    /// be healed even though the instruction that ultimately uses the stale value has no memory
    /// operand naming the slot directly, and even though the exact bit pattern read from the slot
    /// is no longer what is in the register by the time it is used. See
    /// [`fork_verify::on_single_step`]'s case (2c)/(4) for the full reasoning, including why this
    /// is restricted to a single register carrying a chain of *purely additive-constant* updates
    /// from one specific load (never an unbounded history, never more than one register, never an
    /// update that folds in another register's value) -- a false match against a stale, unrelated
    /// earlier read, or against ordinary pointer-to-pointer-style double indirection that does not
    /// actually chain back to the same slot, would risk the same false-positive hazard case (3)'s
    /// doc comment describes.
    fork_verify_last_load: Cell<Option<fork_verify::LastLoad>>,
    /// Backing state for the diagnostic code-page watchpoint (`LITEBOX_CODEWATCH=1`); see
    /// [`fork_verify`]'s `codewatch` module. A field here rather than a bare `static`, matching
    /// `WindowsUserland::console_stdin_reader`'s reasoning -- it keeps this diagnostic off the
    /// crate's ratcheted bare-static count, and per-thread is its natural scope anyway (the
    /// `fork()` child arms the ranges on its own thread and is the thread that traps on them).
    codewatch: fork_verify::CodewatchState,
    /// Backing state for the diagnostic `ctx.rip` hardware watchpoint (`LITEBOX_CTXWATCH=1`); see
    /// [`ctxwatch`]. A field here rather than a bare `static`/`thread_local!`, same reasoning as
    /// `codewatch` above: keeps this diagnostic off the crate's ratcheted bare-static count, and
    /// per-thread is its natural scope (each host OS thread has its own debug registers, and only
    /// the thread arming the watchpoint ever needs to recognize/disarm its own).
    ctxwatch: ctxwatch::State,
}

// SAFETY: `TlsState` is always constructed on one thread and handed to another via
// `spawn_thread` (built on the spawning thread so a fault during construction is diagnosable,
// see `thread_start`'s doc comment), then used exclusively by that new thread from
// `run_with_handle` onward -- its `Cell`/raw-pointer fields are never accessed concurrently by
// two threads at once, matching `install_tls`'s own safety contract that `tls` remains valid and
// single-threaded-owned for the duration of its use.
unsafe impl Send for TlsState {}

/// Scratch space (in bytes) reserved below `host_sp` for the `EXCEPTION_RECORD` that
/// `vectored_exception_handler` writes when redirecting to `exception_callback`. Must be
/// large enough to hold a full `EXCEPTION_RECORD` (152 bytes on x86_64) plus alignment slack,
/// and must keep clear of `[host_sp]`/`[host_sp + 8]`, where `run_thread_arch`'s prologue
/// pushes `thread_ctx` -- `exception_callback` (like `syscall_callback` and
/// `interrupt_callback`) reads `thread_ctx` back via `[rsp]`, so `Rsp` is always set to
/// `host_sp` itself, unmodified; the exception record lives in this separate reserve instead
/// of overlapping the `Rsp` landing spot.
///
/// Must ALSO stay clear of `exception_handler`'s own stack frame: `exception_callback` sets
/// `Rsp = host_sp` before calling it (see above), so `exception_handler`'s locals grow downward
/// from the exact same address this reserve is computed relative to. A too-small reserve here
/// lets that frame's own stack usage overlap and overwrite the just-written record before
/// `exception_handler` ever reads it. Confirmed live (root-caused via a write-then-immediate-
/// readback diagnostic in `vectored_exception_handler`, matching the written code, paired with a
/// diagnostic at `exception_handler`'s own first line reading back a DIFFERENT code at the exact
/// same address, with no intervening exception dispatch and `EFLAGS.TF` confirmed clear the
/// whole time -- ruling out re-entrancy, leaving frame-overlap as the only remaining
/// explanation): `/bin/sh` executing a script FILE (not `-c "..."`, which never reproduced this)
/// drives `fork_verify`'s single-step verification deep enough, combined with a debug-build
/// (unoptimized, larger-than-release) `exception_handler` frame -- itself containing a sizeable
/// `LITEBOX_DIAG_MALLOCNG`-gated diagnostic block with several local buffers plus the full
/// exception-dispatch `match` -- to exceed the previous 4096-byte reserve and corrupt
/// `ExceptionCode` before it was read, surfacing as a spurious "Unhandled Win32 exception code"
/// panic for what was actually a legitimate, already-handled `STATUS_PRIVILEGED_INSTRUCTION`
/// mallocng trap.
///
/// Widened generously (16x) rather than heap-allocating the record: this scratch write happens
/// on the exception-dispatch hot path, potentially while the guest's own allocator lock is held
/// (the mallocng trap this bug was found via is itself an allocator-internal assertion) --
/// allocating here risks reentering a possibly-already-locked allocator. A fixed, generously-
/// sized stack-relative reserve avoids that risk entirely; `exception_handler`'s frame is a
/// fraction of 64KiB even accounting for every diagnostic branch's locals.
const EXCEPTION_RECORD_RESERVE: usize = 65536;

/// Bytes of host stack `vectored_exception_handler_entry` reserves for EACH nesting level before
/// `call`ing `vectored_exception_handler`.
///
/// This is a whole stack frame per level, not a save-slot. The trampoline saves three values
/// (guest `rsp`/`rbp` and `r8`) at `[rsp + 32..56)` plus the callee's 32-byte shadow space, but
/// the callee's OWN frame -- and every frame beneath it: `fork_verify::on_single_step`'s
/// instruction decode, the `VirtualQuery` diagnostic blocks, `eprintln!`'s formatting machinery --
/// grows DOWNWARD from `rsp`. So the distance between one level's slot and the next must cover
/// that entire subtree, not just the saved registers.
///
/// It was previously 64, which covered only the saved registers. At `veh_depth == 1` the nested
/// invocation therefore started its frame 64 bytes below the outer invocation's, while the outer
/// frame was kilobytes deep -- the nested handler's locals wrote straight through the outer
/// handler's live frame. Confirmed live via a `touch` repro (`veh_depth=0x1` at the crash): the
/// outer invocation read back an `ExceptionCode` of `0x470041`, which is not a Windows status
/// code at all -- severity bits `00` mark it a SUCCESS code, and its four bytes are UTF-16LE
/// `"AG"`, i.e. raw string data from the nested frame -- and then dispatched on the equally
/// garbage `rip` beside it (`0x22`, `0x40`), producing the wild-jump cascade that terminates at
/// the `[diag-unrecov-av-giveup]` circuit breaker.
///
/// 8 KiB, from MEASURING the chain rather than estimating it.
///
/// The previous 4 KiB was chosen from `EXCEPTION_RECORD_RESERVE`'s prose ("a fraction of 64KiB
/// even accounting for every diagnostic branch's locals", "with room to spare"). Disassembly says
/// otherwise. `vectored_exception_handler`'s own prologue is eight `push`es plus
/// `sub rsp, 0x9A8` -- 2544 bytes with its return address -- and it calls
/// `fork_verify::on_single_step`, whose prologue is eight `push`es plus `sub rsp, 0x518`, another
/// 1384. That is 3928 bytes for those two frames ALONE, against a 4096-byte stride, before
/// iced-x86's decoder and `InstructionInfoFactory`, the `VirtualQuery` diagnostic blocks, or any
/// `eprintln!` formatting beneath them. The stride was not "room to spare"; it was 168 bytes short
/// of two frames, and whether it overflowed depended on which branches the outer invocation took.
///
/// That is exactly the intermittent, load-dependent corruption observed: `mate-session` crashing
/// with `veh_depth=2`, the trampoline faulting on `mov r8, [rsp+0x30]` immediately AFTER its
/// `call` returned -- the slot it had written before the call, still readable then -- with `rsp`
/// and `rax` both holding `0xfffffffffffffffe`. That value is not arbitrary: BOTH frames above
/// write it into their own locals (`mov qword ptr [rbp+0x908], 0FFFFFFFFFFFFFFFEh` and
/// `[rbp+0x490]` respectively), so the corrupted registers were reading raw bytes out of a nested
/// handler's frame -- the same signature as the 64-byte-stride bug this constant was raised to fix
/// once already, one level up.
///
/// The total depth below `host_sp` is deliberately UNCHANGED at `(CAP + 1) * STRIDE`: the frames
/// already reach further down than the thread's real committed stack (`EXCEPTION_RECORD_RESERVE`'s
/// own doc comment records ~16 KiB as the measured figure), so buying headroom by reaching deeper
/// would trade one hazard for another. The space comes from the exception-record slots instead,
/// which were provisioned for 204 levels against a cap of 7.
///
/// RAISED AGAIN to 16 KiB (from 8 KiB, alongside lowering [`VEH_DEPTH_CAP`] from 3 to 1, keeping
/// `(CAP + 1) * STRIDE` unchanged at 32 KiB -- the same "don't reach deeper below `host_sp`" limit
/// this constant's own history already established) after live-reproducing the pipe-relay-SIGPIPE
/// investigation's blocking crash: a real `LITEBOX_PROCESS_FORK=1` cross-process fork child
/// (`seq`/`sort`/`tail` from `seq 1 200000 | sort -n | tail -3`) hit `[diag-veh-frame-stride-
/// overflow]` on its VERY FIRST pipeline run, 100% reproducible, on all three forked children
/// independently. Temporary instrumentation (`[diag-veh-canary-new]`, since removed) showed the
/// nesting depth was uniformly 1 across all three processes and over 20,000 single-step
/// exceptions -- `veh_depth` never once reached 2, let alone the old cap of 3 -- so the extra
/// levels bought no real headroom for this workload, while `fork_verify::on_single_step`'s
/// cross-process/identity-relocation code path (exercised on every guest instruction fetch until
/// each code page is healed once, unlike the thread-based fork path's sparser translation pattern)
/// reliably needed more than the roughly 5.6 KiB of headroom 8 KiB left after the two measured
/// frame prologues -- reproducibly overflowing into the SAME relative stack offset each time
/// (hence the identical `corrupted_value=0x40` on every hit, not a random garbage read).
/// Redistributing the same 32 KiB ceiling from an unused-in-practice third nesting level to the
/// one depth this workload actually uses fixes the crash without increasing how far this
/// mechanism reaches below `host_sp`.
const VEH_FRAME_STRIDE: u32 = 16384;

/// Maximum nesting depth `vectored_exception_handler_entry`'s per-depth frame (see
/// `VEH_FRAME_STRIDE`) will use before giving up on the host-stack swap and bailing out via
/// `.Lsearch`.
///
/// The trampoline's per-depth frames occupy
/// `[host_sp - VEH_FRAME_STRIDE * (VEH_DEPTH_CAP + 1), host_sp)`, growing down; the
/// exception-record slots occupy the bottom of the same `EXCEPTION_RECORD_RESERVE` region,
/// growing up from `host_sp - EXCEPTION_RECORD_RESERVE` (see the `EXC_RECORD_SLOT_SIZE`/
/// `EXC_RECORD_SLOTS` computation near `exception_record_ptr`'s definition). Both of those
/// non-overlap requirements are enforced by `const` assertions at that definition rather than
/// asserted only in prose -- the previous prose claim that the two ranges were "comfortably
/// clear" was wrong by 64 bytes at the deepest level, and the slot stride it described (128) was
/// itself smaller than the 152-byte `EXCEPTION_RECORD` it was sizing.
///
/// Now that each level costs a real 4 KiB frame rather than 64 bytes, the cap that fits the same
/// already-committed reserve is 7, not 512. That is not a regression in robustness: the old 512
/// never actually gave 512 usable levels, it gave one usable level and 511 that silently
/// corrupted each other. A fault nesting deeper than a handful of levels indicates a separate,
/// still-unexplained bug (a live `LITEBOX_PROCESS_FORK=1` repro was once observed reaching depth
/// ~3271 under the older single-fixed-slot bug), and `.Lsearch` is the correct, honest response
/// to it -- not a deeper stack of frames that overlap anyway.
/// Lowered from 7 alongside the doubling of [`VEH_FRAME_STRIDE`], so `(CAP + 1) * STRIDE` -- how
/// far below `host_sp` the deepest frame reaches -- stays exactly where it was at 32 KiB. Observed
/// nesting in practice is 1-2; a cap of 3 covers that with a level in hand, and a fault nesting
/// deeper than that is the separate, still-unexplained condition this constant's original comment
/// already describes, where `.Lsearch` is the honest answer.
///
/// LOWERED AGAIN to 1 (from 3), alongside doubling [`VEH_FRAME_STRIDE`], per that constant's own
/// doc comment: a live cross-process-fork repro (`seq 1 200000 | sort -n | tail -3` under
/// `LITEBOX_PROCESS_FORK=1`) recorded `veh_depth` over 20,000 times across three independent
/// forked children and never once saw depth 2, so the "1-2" estimate above was optimistic for this
/// workload -- real nesting is 1, reliably, and every one of those depth-1 invocations needs more
/// of the shared 32 KiB budget than a cap of 3 could spare it. A fault that genuinely nests to
/// depth 2 now takes `.Lsearch` immediately instead of getting its own slice, which is a narrower
/// safety net than before -- but the prior net still could not survive the depth-1 case this
/// workload actually exercises, so this is a strictly better trade until real reentrant nesting is
/// independently observed and budgeted for.
const VEH_DEPTH_CAP: u32 = 1;

/// Diagnostic-only: how many times `vectored_exception_handler_entry`'s `.Lsearch` path has
/// returned `EXCEPTION_CONTINUE_SEARCH` straight from the naked-asm trampoline, without ever
/// entering `vectored_exception_handler`.
///
/// This exit is the single blind spot in the whole fault path: because Rust never runs, the
/// exception table is never consulted, and none of the `[diag-unrecov-av]`-family diagnostics can
/// observe that it happened. An access violation with a perfectly valid, covering exception-table
/// entry that takes this exit is silently unrecoverable, which is exactly the "a covering entry
/// exists but recovery never happens" shape this investigation is chasing.
///
/// Incremented by a `lock inc` directly in the trampoline (see `.Lsearch`); read only by
/// diagnostics. `u64` because the asm increments a full QWORD.
static LSEARCH_EXIT_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Track-B investigation (fork-without-exec hang): armed (set to the current tick count) by
/// `vectored_exception_handler`'s unrecovered-AV path immediately BEFORE it attempts to end the
/// process (`TerminateProcess`/`RaiseFailFastException`), and read by `fault_terminate_watchdog`
/// (spawned once at process startup, see `WindowsUserland::new`). Exists because live evidence
/// this session showed BOTH a self-`TerminateProcess(GetCurrentProcess(), ...)` call AND a
/// `RaiseFailFastException` call, made from exactly the VEH thread that is mid-dispatch for the
/// very fault being handled, can fail to actually end the process on this host/Windows build --
/// confirmed directly: the diagnostic print immediately before each attempt appeared in the
/// captured log (so the calls were genuinely reached and issued), yet the process was
/// independently observed, 30+ seconds later, still alive with its sole thread still parked at
/// `WaitReason=Suspended`/`HasExited=False` -- while an EXTERNAL `Stop-Process -Force` (a
/// `TerminateProcess` call from a DIFFERENT process) against the same PID succeeded immediately
/// every time. This is consistent with a thread that is itself still inside kernel-mode
/// exception/debug-port delivery for its own fault being unable to reliably terminate that same
/// process from within -- any further self-directed termination call can block behind the very
/// kernel protocol it is trying to escape. Zero (this default) means no fault is in flight;
/// `AtomicU64` so the watchdog can also read WHEN the fault happened, for its bounded grace
/// window.
static FAULT_TERMINATE_ARMED_TICK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

impl TlsState {
    /// Creates a new `TlsState` with all fields zeroed / defaulted.
    fn new() -> Self {
        Self {
            host_sp: Cell::new(core::ptr::null_mut()),
            host_bp: Cell::new(core::ptr::null_mut()),
            guest_context_top: core::ptr::null_mut::<litebox_common_linux::PtRegs>().into(),
            guest_xmm0_5: Cell::new([0; 6]),
            guest_fs_base: Cell::new(0),
            scratch: 0.into(),
            veh_depth: Cell::new(0),
            is_in_guest: false.into(),
            interrupt: false.into(),
            continue_context: Box::default(),
            pending_host_signals: AtomicU32::new(0),
            waiting_waker: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
            has_entered_guest: false.into(),
            fork_verify: RefCell::new(None),
            fork_verify_step_count: Cell::new(0),
            fork_verify_av_rip_repeat: Cell::new(None),
            fork_verify_step_rip_repeat: Cell::new(None),
            fork_verify_last_load: Cell::new(None),
            codewatch: fork_verify::CodewatchState::new(),
            ctxwatch: ctxwatch::State::new(),
        }
    }
}

/// Stores `tls` in the current thread's Windows TLS slot.
///
/// # Safety
///
/// The caller must ensure `tls` remains valid for the duration of its use.
unsafe fn install_tls(tls: &TlsState) {
    let tls_index = TLS_INDEX.load(Ordering::Relaxed);
    unsafe {
        windows_sys::Win32::System::Threading::TlsSetValue(
            tls_index,
            core::ptr::from_ref(tls).cast(),
        );
    }
}

/// Clears the current thread's Windows TLS slot.
fn uninstall_tls() {
    let tls_index = TLS_INDEX.load(Ordering::Relaxed);
    unsafe { windows_sys::Win32::System::Threading::TlsSetValue(tls_index, core::ptr::null()) };
}

fn get_tls_ptr() -> Option<*const TlsState> {
    let tls_index = TLS_INDEX.load(Ordering::Relaxed);
    if tls_index == u32::MAX {
        return None;
    }
    let ptr =
        unsafe { windows_sys::Win32::System::Threading::TlsGetValue(tls_index).cast::<TlsState>() };
    if ptr.is_null() {
        return None;
    }
    Some(ptr)
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
unsafe extern "C-unwind" fn run_thread_arch(thread_ctx: &mut ThreadContext, tls_state: &TlsState) {
    core::arch::naked_asm!(
    "
    .seh_proc run_thread
    // Push all non-volatiles
    push rbp
    .seh_pushreg rbp
    mov rbp, rsp
    .seh_setframe rbp, 0
    push rbx
    .seh_pushreg rbx
    push rdi
    .seh_pushreg rdi
    push rsi
    .seh_pushreg rsi
    push r12
    .seh_pushreg r12
    push r13
    .seh_pushreg r13
    push r14
    .seh_pushreg r14
    push r15
    .seh_pushreg r15
    sub rsp, 168 // align + space for xmm6-xmm15
    .seh_stackalloc 168
    movdqa [rsp + 0*16], xmm6
    .seh_savexmm xmm6, 0*16
    movdqa [rsp + 1*16], xmm7
    .seh_savexmm xmm7, 1*16
    movdqa [rsp + 2*16], xmm8
    .seh_savexmm xmm8, 2*16
    movdqa [rsp + 3*16], xmm9
    .seh_savexmm xmm9, 3*16
    movdqa [rsp + 4*16], xmm10
    .seh_savexmm xmm10, 4*16
    movdqa [rsp + 5*16], xmm11
    .seh_savexmm xmm11, 5*16
    movdqa [rsp + 6*16], xmm12
    .seh_savexmm xmm12, 6*16
    movdqa [rsp + 7*16], xmm13
    .seh_savexmm xmm13, 7*16
    movdqa [rsp + 8*16], xmm14
    .seh_savexmm xmm14, 8*16
    movdqa [rsp + 9*16], xmm15
    .seh_savexmm xmm15, 9*16
    .seh_endprologue

    // Offset into the TEB (gs segment) where TLS slots are stored.
    .equ TEB_TLS_SLOTS_OFFSET, 5248

    push    rcx // Alignment
    push    rcx // Save thread_ctx

    // Save the host rsp and rbp into the TLS state.
    mov     QWORD PTR [rdx + {HOST_SP}], rsp
    mov     QWORD PTR [rdx + {HOST_BP}], rbp

    call {init_handler}
    jmp .Ldone

    // This entry point is called from the guest when it issues a syscall
    // instruction.
    //
    // At entry, the register context is the guest context with the
    // return address in rcx. r11 is an available scratch register (it would
    // contain rflags if the syscall instruction had actually been issued).
    .globl  syscall_callback
syscall_callback:
    // Get the TLS state from the TLS slot, save the guest's own rsp into TlsState, and switch
    // rsp to the host-owned guest-context stack BEFORE touching the real stack pointer in any
    // way (no push/pop, no [rsp]-relative access of any kind up to this point). This ordering
    // is load-bearing: a guest thread that just munmap'd its own stack as the last step of
    // musl's `pthread_exit`/`__unmapself` idiom (real Linux's own version of this idiom
    // deliberately touches zero stack bytes between the munmap and its own exit syscall, which
    // is what makes it safe there) reaches this trampoline for that immediately-following exit
    // syscall with `rsp` still pointing into the region it just unmapped -- any push/pop before
    // this switch (the previous code had `pushfq`/`and`/`popfq` here first) writes into freed,
    // decommitted memory and raises an unhandled, VEH-invisible access violation. Every
    // instruction below, up to and including the `mov rsp, ...`, is register/memory-operand
    // only (`[r11 + ...]`, `[rip + ...]`, `gs:[...]`) and never dereferences `[rsp]` itself, so
    // it is safe regardless of whether the guest's own stack is still mapped.
    mov     r11d, DWORD PTR [rip + {TLS_INDEX}]
    mov     r11, QWORD PTR gs:[r11 * 8 + TEB_TLS_SLOTS_OFFSET]
    mov     QWORD PTR [r11 + {SCRATCH}], rsp
    mov     rsp, QWORD PTR [r11 + {GUEST_CONTEXT_TOP}]

    // Clear EFLAGS.TF in the live CPU flags. The guest reaches here via a call (the syscall
    // rewriter's trampoline for every guest syscall instruction, not a real syscall), which is
    // itself the next instruction a fork() child under fork_verify single-step verification was
    // stepped through -- so if TF was armed, it is still live in the CPU's real flags register
    // at this point, and every subsequent host instruction here (the register spills below, the
    // call into the syscall handler, ...) would otherwise raise its own single-step trap while
    // is_in_guest is about to be (or has just been) cleared, i.e. exactly the state
    // vectored_exception_handler does not have a fork_verify handler for -- an unhandled
    // EXCEPTION_SINGLE_STEP (STATUS_SINGLE_STEP, 0x80000004) that tears down the whole host
    // process instead of just the child. pushfq/and/popfq now runs on the host-owned stack
    // (rsp was already switched above), so it is always safe regardless of the guest stack's
    // state.
    pushfq
    and     QWORD PTR [rsp], 0xfffffffffffffeff
    popfq
    // Clear the in-guest flag.
    mov     BYTE PTR [r11 + {IS_IN_GUEST}], 0
    // Save the guest's caller-saved xmm0-xmm5 into TlsState before any other host code (which
    // is free to clobber them) runs. xmm6-xmm15 are already protected for the whole guest-thread
    // lifetime by run_thread_arch's own prologue/epilogue; these are the remaining, previously
    // unsaved ones. Must happen before the `call {syscall_handler}` below.
    movups  XMMWORD PTR [r11 + {GUEST_XMM0_5} + 0*16], xmm0
    movups  XMMWORD PTR [r11 + {GUEST_XMM0_5} + 1*16], xmm1
    movups  XMMWORD PTR [r11 + {GUEST_XMM0_5} + 2*16], xmm2
    movups  XMMWORD PTR [r11 + {GUEST_XMM0_5} + 3*16], xmm3
    movups  XMMWORD PTR [r11 + {GUEST_XMM0_5} + 4*16], xmm4
    movups  XMMWORD PTR [r11 + {GUEST_XMM0_5} + 5*16], xmm5

    // Save caller-saved registers
    push    0x2b       // pt_regs->ss = __USER_DS
    push    QWORD PTR [r11 + {SCRATCH}] // pt_regs->sp
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
    push    r12
    push    r13
    push    r14
    push    r15

    /// Reestablish the stack and frame pointers.
    mov     rsp, [r11 + {HOST_SP}]
    mov     rbp, [r11 + {HOST_BP}]

    // Handle the syscall. This will jump back to the guest but
    // will return if the thread is exiting.
    mov  rcx, QWORD PTR [rsp] // thread_ctx
    call {syscall_handler}
    jmp .Ldone

exception_callback:
    // Handle the exception. The stack and frame pointers are already restored,
    // and the guest context is up to date. rcx contains a pointer to the
    // guest pt_regs, and rdx contains a pointer to the exception record.
    mov  rcx, QWORD PTR [rsp] // thread_ctx
    call {exception_handler}
    jmp .Ldone

interrupt_callback:
    mov  rcx, QWORD PTR [rsp] // thread_ctx
    call {interrupt_handler}
    jmp .Ldone

.Ldone:
    // Restore non-volatile registers and return.
    lea  rsp, [rbp - (168 + 56)]
    movdqa xmm6, [rsp + 0*16]
    movdqa xmm7, [rsp + 1*16]
    movdqa xmm8, [rsp + 2*16]
    movdqa xmm9, [rsp + 3*16]
    movdqa xmm10, [rsp + 4*16]
    movdqa xmm11, [rsp + 5*16]
    movdqa xmm12, [rsp + 6*16]
    movdqa xmm13, [rsp + 7*16]
    movdqa xmm14, [rsp + 8*16]
    movdqa xmm15, [rsp + 9*16]
    add rsp, 168 // 10 * 16 + 8 (for stack alignment)
    pop  r15
    pop  r14
    pop  r13
    pop  r12
    pop  rsi
    pop  rdi
    pop  rbx
    pop  rbp
    ret
    .seh_endproc
    ",
    init_handler = sym init_handler,
    syscall_handler = sym syscall_handler,
    exception_handler = sym exception_handler,
    interrupt_handler = sym interrupt_handler,
    TLS_INDEX = sym TLS_INDEX,
    HOST_SP = const core::mem::offset_of!(TlsState, host_sp),
    HOST_BP = const core::mem::offset_of!(TlsState, host_bp),
    GUEST_CONTEXT_TOP = const core::mem::offset_of!(TlsState, guest_context_top),
    GUEST_XMM0_5 = const core::mem::offset_of!(TlsState, guest_xmm0_5),
    SCRATCH = const core::mem::offset_of!(TlsState, scratch),
    IS_IN_GUEST = const core::mem::offset_of!(TlsState, is_in_guest),
    );
}

thread_local! {
    /// All per-thread state for the `LITEBOX_DIAG_WAIT4GATE` diagnostics, in one thread-local.
    static DIAG: RefCell<DiagState> = const { RefCell::new(DiagState::new()) };
}

/// Number of recent guest resumes [`DiagState::history`] retains per thread.
const DIAG_RESUME_HISTORY_LEN: usize = 8;

/// One recorded guest resume: `(orig_rax, rip, rcx, rsp, rax)`.
type DiagResume = (usize, usize, usize, usize, usize);

/// Per-thread state backing the `LITEBOX_DIAG_WAIT4GATE` diagnostics.
///
/// Purely a debugging aid, and entirely inert unless that environment variable is set: nothing
/// here is required for correctness, and no field is read except by the diagnostic prints in
/// [`vectored_exception_handler`].
struct DiagState {
    /// Which resume path (`"sysret"`/`"ntcontinue"`) this thread last took, so a crash handler
    /// can report which one was in effect for the resume immediately preceding a fault.
    last_resume_path: &'static str,
    /// Ring buffer of this thread's most recent resumes, plus the running total, so a crash
    /// handler can print the exact sequence of resumes leading up to a fault.
    history: (usize, [DiagResume; DIAG_RESUME_HISTORY_LEN]),
    /// Pass-30: `faulting_rsp - 8` recorded by the `[diag-rip0]` block, consumed once (after
    /// `ctxwatch::disarm()` runs later in the same `vectored_exception_handler` call) to arm the
    /// reactive guest-stack-slot watchpoint. `None` when no fault has queued an address yet, or
    /// after it has already been consumed.
    pending_watch_addr: Option<usize>,
}

impl DiagState {
    const fn new() -> Self {
        Self {
            last_resume_path: "none",
            history: (0, [(0, 0, 0, 0, 0); DIAG_RESUME_HISTORY_LEN]),
            pending_watch_addr: None,
        }
    }
}

/// Diagnostic-only (`LITEBOX_DIAG_WAIT4GATE=1`): records `faulting_rsp - 8` -- the guest stack
/// slot proven (pass 28/29) to hold the bad zero a null-branching guest read -- for the reactive
/// watchpoint armed later in the same `vectored_exception_handler` call, after `ctxwatch::disarm()`
/// runs.
fn diag_pending_watch_addr(faulting_rsp: u64) {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "diagnostic-only; this platform is x86_64-only, so rsp fits in usize"
    )]
    let addr = (faulting_rsp as usize).wrapping_sub(8);
    DIAG.with_borrow_mut(|d| d.pending_watch_addr = Some(addr));
}

/// Diagnostic-only (`LITEBOX_DIAG_WAIT4GATE=1`): takes (clears) this thread's pending reactive
/// watch address, if any, set by [`diag_pending_watch_addr`] earlier in the same VEH call.
fn diag_take_pending_watch_addr() -> Option<usize> {
    DIAG.with_borrow_mut(|d| d.pending_watch_addr.take())
}

/// Diagnostic-only (`LITEBOX_DIAG_WAIT4GATE=1`): records one guest resume, along with which path
/// it took, into this thread's [`DiagState`].
fn diag_record_resume(path: &'static str, ctx: &litebox_common_linux::PtRegs) {
    DIAG.with_borrow_mut(|d| {
        d.last_resume_path = path;
        let n = d.history.0;
        d.history.1[n % DIAG_RESUME_HISTORY_LEN] =
            (ctx.orig_rax, ctx.rip, ctx.rcx, ctx.rsp, ctx.rax);
        d.history.0 = n.wrapping_add(1);
    });
}

/// Diagnostic-only: formats this thread's last resume path and recent-resume history, oldest
/// first.
fn diag_resume_history() -> String {
    use core::fmt::Write as _;
    DIAG.with_borrow(|d| {
        let (total, ring) = d.history;
        let shown = total.min(DIAG_RESUME_HISTORY_LEN);
        let mut out = format!("last_resume_path={} total={total}", d.last_resume_path);
        for i in (0..shown).rev() {
            let (orig_rax, rip, rcx, rsp, rax) = ring[(total - 1 - i) % DIAG_RESUME_HISTORY_LEN];
            let _ = write!(
                out,
                " | -{i}: orig_rax={orig_rax:#x} rip={rip:#x} rcx={rcx:#x} rsp={rsp:#x} rax={rax:#x}"
            );
        }
        out
    })
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
///
/// Whether `ctx` could plausibly be resumed as GUEST state.
///
/// `rip` and `rsp` must both land inside the guest's own address range. A value outside it did not
/// come from guest execution: observed live as `rip=0x7ff8b1b221f4`, a Windows system-DLL address,
/// sitting in a context about to be resumed as if it were guest code.
///
/// This is a necessary condition, not a sufficient one -- it cannot tell a corrupted in-range
/// address from a good one. It exists to catch the case that is structurally unrecoverable:
/// resuming onto an out-of-range `rip`/`rsp` faults with no usable exception frame (once `rsp`
/// itself is bad, the CPU cannot even push one), which the exception-table recovery cannot help
/// with.
fn guest_context_is_plausible(ctx: &litebox_common_linux::PtRegs) -> bool {
    use litebox::platform::PageManagementProvider;
    let task_min = <WindowsUserland as PageManagementProvider<0x1000>>::TASK_ADDR_MIN;
    let task_max = <WindowsUserland as PageManagementProvider<0x1000>>::TASK_ADDR_MAX;
    (task_min..task_max).contains(&ctx.rip) && (task_min..task_max).contains(&ctx.rsp)
}

unsafe extern "C" fn switch_to_guest(ctx: &litebox_common_linux::PtRegs) -> ! {
    #[unsafe(naked)]
    extern "C" fn switch_to_guest_sysret(ctx: &litebox_common_linux::PtRegs) -> ! {
        // SAFETY/CORRECTNESS NOTE: this function must never repoint the real CPU `rsp`
        // at `ctx`'s own backing memory (a `&PtRegs`, not a real guarded stack) while
        // any GPR field is still unread. Doing so leaves a window in which any
        // synchronous, thread-local event (SEH/VEH dispatch, a debug/trace trap, or
        // any other mechanism that pushes data onto "the current stack") would corrupt
        // not-yet-consumed fields -- including `rip`/`rcx` -- before they are used.
        // Every GPR is therefore addressed directly off `rcx` (the `ctx` pointer, per
        // the `extern "C"` ABI) via fixed offsets matching `PtRegs`'s `#[repr(C)]`
        // field order, and the real `rsp` is set to the guest's `rsp` only in the
        // second-to-last instruction, immediately before the final `jmp`, mirroring
        // the same narrow, unavoidable gap the original fast path already had at its
        // very end.
        core::arch::naked_asm!(
            "switch_to_guest_start:",
            // `rcx` (the `ctx` pointer, per the extern "C" ABI) is the base for every
            // field read below, addressed by fixed offset matching `PtRegs`'s
            // `#[repr(C)]` field order: r15=0x00 r14=0x08 r13=0x10 r12=0x18 rbp=0x20
            // rbx=0x28 r11=0x30 r10=0x38 r9=0x40 r8=0x48 rax=0x50 rcx=0x58 rdx=0x60
            // rsi=0x68 rdi=0x70 orig_rax=0x78 rip=0x80 cs=0x88 eflags=0x90 rsp=0x98.
            //
            // The real `rsp` is never repointed at `ctx`'s own backing memory --
            // every GPR is loaded directly into its final register while `rsp` still
            // refers to the real (host) stack, which remains valid the entire time,
            // so any synchronous, thread-local event (SEH/VEH dispatch, a debug/trace
            // trap, etc.) that lands during this window pushes onto real stack
            // memory, never onto `ctx`'s fields. `rcx` itself (the base pointer) is
            // the very last register loaded, immediately before the jump, the same
            // way the original fast path only set the real `rsp` immediately before
            // its own final `jmp rcx`. Like the original fast path, this relies on
            // the sysret-entry invariant `ctx.rcx == ctx.rip` (checked by the caller
            // before choosing this path): `rcx` is used as the `ctx` base pointer for
            // every field read, then overwritten with `ctx.rip` (equal to `ctx.rcx`
            // by that invariant) as its own final value immediately before the jump.
            "mov r15, [rcx + 0x00]",
            "mov r14, [rcx + 0x08]",
            "mov r13, [rcx + 0x10]",
            "mov r12, [rcx + 0x18]",
            "mov rbp, [rcx + 0x20]",
            "mov rbx, [rcx + 0x28]",
            "mov r11, [rcx + 0x30]",
            "mov r10, [rcx + 0x38]",
            "mov r9,  [rcx + 0x40]",
            "mov r8,  [rcx + 0x48]",
            "mov rax, [rcx + 0x50]",
            "mov rdx, [rcx + 0x60]",
            "mov rsi, [rcx + 0x68]",
            "mov rdi, [rcx + 0x70]",
            // Stage and restore `eflags` on the still-valid real (host) stack --
            // ordinary `push`/`popfq` here are no different from any other function
            // using its own stack; it is not the hazardous "rsp points into a
            // struct" pattern, since `rsp` itself has not moved yet. Only once
            // `eflags` is fully restored does `rsp` adopt the guest's real value, in
            // a single `mov`, immediately followed by the jump -- the same narrow,
            // unavoidable gap the original fast path already had at its own end.
            "push qword ptr [rcx + 0x90]", // eflags
            "popfq",                       // restore guest eflags, from the real host stack
            "mov rsp, [rcx + 0x98]",       // adopt the guest's real rsp
            "mov rcx, [rcx + 0x80]",       // guest rip -> rcx (also satisfies the sysret
            // ABI invariant that rcx == rip on guest entry)
            "jmp rcx", // jump to guest rip
            "switch_to_guest_end:",
        );
    }

    fn switch_to_guest_ntcontinue(tls: &TlsState, ctx: &litebox_common_linux::PtRegs) -> ! {
        use litebox::utils::ReinterpretSignedExt;
        use windows_sys::Win32::System::Diagnostics::Debug::{
            CONTEXT, CONTEXT_CONTROL_AMD64, CONTEXT_INTEGER_AMD64,
        };
        #[link(name = "ntdll")]
        unsafe extern "system" {
            fn NtContinue(
                ctx: *const CONTEXT,
                raise_alert: u8,
            ) -> windows_sys::Win32::Foundation::NTSTATUS;
        }
        let win_ctx = tls.continue_context.get();
        // SAFETY: no other code accesses `continue_context` while `is_in_guest` is false.
        unsafe {
            win_ctx.write(CONTEXT {
                ContextFlags: CONTEXT_CONTROL_AMD64 | CONTEXT_INTEGER_AMD64,
                // `EFLAGS.TF` is never present in `ctx.eflags` (it is masked out of every
                // guest-visible eflags value); it is added here, and only here, when this thread
                // is a `fork()` child under verification -- arming the single-step trap that
                // `fork_verify` uses to inspect each of the child's instructions.
                EFlags: (ctx.eflags | fork_verify::entry_eflags_tf(tls)).trunc(),
                Rax: ctx.rax as u64,
                Rcx: ctx.rcx as u64,
                Rdx: ctx.rdx as u64,
                Rbx: ctx.rbx as u64,
                Rsp: ctx.rsp as u64,
                Rbp: ctx.rbp as u64,
                Rsi: ctx.rsi as u64,
                Rdi: ctx.rdi as u64,
                R8: ctx.r8 as u64,
                R9: ctx.r9 as u64,
                R10: ctx.r10 as u64,
                R11: ctx.r11 as u64,
                R12: ctx.r12 as u64,
                R13: ctx.r13 as u64,
                R14: ctx.r14 as u64,
                R15: ctx.r15 as u64,
                Rip: ctx.rip as u64,
                // `CONTEXT_CONTROL_AMD64` covers `SegCs`/`SegSs` in addition to
                // `Rip`/`Rsp`/`Rbp`/`EFlags` (all of which this literal already sets) -- omitting
                // them here left them at `CONTEXT::default()`'s zero, i.e. every `NtContinue`-path
                // resume (every thread's very first resume, and every resume of a `fork()` child
                // under verification) asked the kernel to establish an invalid/null code and stack
                // segment selector instead of the guest's real user-mode `0x33`/`0x2b` (see
                // `litebox_common_linux::arch::USER_CS`/`USER_DS`, which `PtRegs::cs`/`ss` are
                // always populated with). Populate them explicitly from `ctx.cs`/`ctx.ss`, the
                // same values the `switch_to_guest_sysret` fast path already restores correctly
                // via its own `popfq`-preserved segment state. Segment selectors are always
                // 16-bit values (`ctx.cs`/`ctx.ss` are `usize` only for uniform `PtRegs` field
                // typing); the exact values written here are always `arch::USER_CS`/`USER_DS`
                // (`0x33`/`0x2b`), which trivially fit.
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "segment selectors are always 16-bit values (USER_CS/USER_DS)"
                )]
                SegCs: ctx.cs as u16,
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "segment selectors are always 16-bit values (USER_CS/USER_DS)"
                )]
                SegSs: ctx.ss as u16,
                ..CONTEXT::default()
            });
        }
        // Ensure the context is written before we set `is_in_guest` so that
        // `ThreadHandle::interrupt` can see a consistent state.
        std::sync::atomic::compiler_fence(Ordering::Release);
        tls.is_in_guest.set(true);
        unsafe {
            let status = NtContinue(win_ctx, 0);
            panic!(
                "NtContinue failed: {}",
                std::io::Error::from_raw_os_error(
                    windows_sys::Win32::Foundation::RtlNtStatusToDosError(status)
                        .reinterpret_as_signed(),
                ),
            );
        }
    }

    let tls = unsafe { &*get_tls_ptr().expect("TLS not initialized") };
    assert!(!tls.is_in_guest.get());

    // A `ctx.rip`/`ctx.rsp` that falls outside the guest's own valid address range must never
    // reach either resume path below -- both end in an unconditional jump/stack-adoption with no
    // further checks, so a corrupted value here (e.g. a small integer like `2` that leaked in
    // from unvalidated guest-memory state during `sigreturn`, or any other not-yet-discovered
    // source) would otherwise silently jump/switch onto it, producing a fault whose own exception
    // frame the CPU cannot even push (once `rsp` itself is the bad value), which this project's
    // exception-table recovery is then structurally unable to help with -- live-captured this
    // session as an infinite identical-`rip` retry loop culminating in a genuine host stack
    // overflow (see AGENTS.md's 72nd/73rd/74th passes) rather than an ordinary guest-visible
    // SIGSEGV. Fail loudly and immediately here instead: a real Linux kernel would deliver SIGSEGV
    // to a userspace program that corrupts its own signal frame this way rather than crash the
    // kernel itself; this project does not yet synthesize that signal at this choke point (a
    // follow-up), but a diagnostic panic that unambiguously names the corrupted register and its
    // value is a strict improvement over the current silent, unrecoverable, hard-to-diagnose
    // cascade.
    // Unreachable in the normal case: the resume path's caller has already turned an implausible
    // context into a guest SIGSEGV (see `guest_context_is_plausible`). This stays as a last-ditch
    // assertion because `switch_to_guest` is also reached from paths that do not go through that
    // caller, and jumping onto a corrupted `rip`/`rsp` is unrecoverable by construction.
    assert!(
        guest_context_is_plausible(ctx),
        "switch_to_guest: refusing to resume with an implausible guest address \
         (rip={:#x} rsp={:#x}) -- this would otherwise jump/switch onto a corrupted value with \
         no further checks",
        ctx.rip,
        ctx.rsp,
    );

    // Restore fsbase for the guest.
    WindowsUserland::restore_thread_fs_base();

    // Restore the guest's xmm0-xmm5 (the caller-saved SSE registers, see `guest_xmm0_5`'s doc
    // comment) as late as possible, immediately before handing control to either resume path --
    // no host Rust code runs between this and the jump into guest code on either path, so
    // nothing here can re-clobber them.
    {
        let xmm = tls.guest_xmm0_5.get();
        unsafe {
            core::arch::asm!(
                "movups xmm0, [{p} + 0*16]",
                "movups xmm1, [{p} + 1*16]",
                "movups xmm2, [{p} + 2*16]",
                "movups xmm3, [{p} + 3*16]",
                "movups xmm4, [{p} + 4*16]",
                "movups xmm5, [{p} + 5*16]",
                p = in(reg) xmm.as_ptr(),
                out("xmm0") _, out("xmm1") _, out("xmm2") _,
                out("xmm3") _, out("xmm4") _, out("xmm5") _,
                options(nostack, readonly),
            );
        }
    }

    // The fast path for switching to the guest relies on rcx == rip. This is
    // the common case, because the syscall instruction sets rcx to rip at entry
    // to the kernel. When this is not the case, we use NtContinue to jump to
    // the guest with the full register state.
    //
    // This is much slower, but it is only used for things like signal handlers,
    // so it should not be on the critical path.
    //
    // The fast path additionally requires this thread to have entered guest mode at least once
    // before: `switch_to_guest_sysret` relies on genuine `sysret`-style CPU semantics that are
    // only established by a PRIOR entry into kernel mode via the `syscall` instruction on this
    // exact thread. A brand-new host thread's first-ever transition (e.g. a `fork()`-created
    // child resuming into a copy of the parent's syscall-entry context, where `rcx == rip` holds
    // only by coincidence) must always take the slower `NtContinue` path instead.
    //
    // A `fork()` child under verification must likewise always take the `NtContinue` path: it is
    // the only one that can set `EFLAGS.TF` (the fast path restores eflags from `ctx`, which
    // never carries TF) to arm the single-step trap `fork_verify` depends on.
    if ctx.rcx == ctx.rip && tls.has_entered_guest.get() && !fork_verify::is_verifying(tls) {
        if diag_rip0_enabled() {
            diag_record_resume("sysret", ctx);
        }
        tls.is_in_guest.set(true);
        switch_to_guest_sysret(ctx)
    } else {
        if diag_rip0_enabled() {
            diag_record_resume("ntcontinue", ctx);
        }
        tls.has_entered_guest.set(true);
        switch_to_guest_ntcontinue(tls, ctx)
    }
}

fn thread_start(
    init_thread: Box<
        dyn litebox::shim::InitThread<ExecutionContext = litebox_common_linux::PtRegs>,
    >,
    mut ctx: litebox_common_linux::PtRegs,
    tls_state: TlsState,
) {
    // `tls_state` is constructed by the SPAWNING thread (see `spawn_thread`), not here: any
    // fault during `TlsState::new()` -- including the heap allocation inside
    // `continue_context: Box::default()` -- would otherwise run on this brand-new OS thread
    // BEFORE `install_tls` (called by `run_with_handle` below) has populated this thread's own
    // Windows TLS slot, hitting the exact same silently-undiagnosable window this function's own
    // `init_thread.init()` comment below already documents for a different call -- confirmed live
    // as a 100%-reproducible whole-host-process crash (`labwc`'s own fontconfig-cache pthread
    // spawn, no `[veh]` trace lines at all) that a fresh `LITEBOX_DIAG_FATALDUMP=1` capture could
    // not explain until this construction-ordering gap was found by direct code reading.
    tls_state
        .guest_context_top
        .set(std::ptr::from_mut(&mut ctx).wrapping_add(1));

    if std::env::var_os("LITEBOX_DIAG_TLS_ADDR").is_some() {
        eprintln!(
            "[diag-tls-addr] pid={} tid={:?} tls_state={:p} (thread_start)",
            std::process::id(),
            std::thread::current().id(),
            &tls_state,
        );
        use std::io::Write;
        let _ = std::io::stderr().flush();
    }

    ThreadHandle::run_with_handle(&tls_state, || {
        // `init_thread.init()` -- which, for a `fork()` child, does real work capable of
        // faulting or arming `EFLAGS.TF` (`ThreadInitState::ForkedChild`'s `sys_arch_prctl`/
        // `begin_fork_child_verification` calls in `litebox_shim_linux`) -- must run strictly
        // AFTER `install_tls` (done by `run_with_handle` above), never before: this thread's
        // Windows TLS slot is otherwise still unpopulated, so `vectored_exception_handler`'s
        // very first check (`get_tls_ptr()`) finds nothing, bails via
        // `EXCEPTION_CONTINUE_SEARCH`, and every one of this file's own carefully-built
        // exception repair paths (the FS_BASE-reset repair, fork_verify's single-step healing)
        // is silently bypassed for any fault raised during that window -- confirmed live as the
        // cause of a 100%-reproducible stack overflow on `sh -c "a; b"` (any two-command shell
        // sequence forking a child): `LITEBOX_VEH_TRACE=1` showed zero `[veh]` trace lines
        // despite a real, dispatched exception, which is only possible via that early-return.
        let shim = init_thread.init();

        #[cfg(target_arch = "x86_64")]
        if std::env::var_os("LITEBOX_DIAG_TLS_ADDR").is_some() {
            eprintln!(
                "[diag-tls-addr] pid={} tid={:?} init_thread.init() returned, about to run_thread_arch rip={:#x} rsp={:#x}",
                std::process::id(),
                std::thread::current().id(),
                ctx.rip,
                ctx.rsp,
            );
            use std::io::Write;
            let _ = std::io::stderr().flush();
        }

        // Allow caller to run some code before we return to the new thread.
        let mut thread_ctx = ThreadContext {
            shim: shim.as_ref(),
            ctx: &mut ctx,
            tls: &tls_state,
        };
        unsafe { run_thread_arch(&mut thread_ctx, &tls_state) };
    });
}

impl litebox::platform::ThreadProvider for WindowsUserland {
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
        // Guest code (both a brand-new thread's entry point and a `fork()` child resuming via
        // `ThreadInitState::ForkedChild`) runs directly on this real Windows thread's own stack --
        // there is no separate emulated guest-stack region (see `switch_to_guest`'s doc comment).
        // `std::thread::Builder`'s default stack size (1 MiB on Windows) is far smaller than a
        // Linux guest program is entitled to assume (`DEFAULT_STACK_SIZE` in
        // `litebox_shim_linux::loader` is 8 MiB, matching real Linux's default `ulimit -s`).
        // Mirror the guest's own expected stack size here so an undersized real host stack is
        // never a needless bottleneck; TODO(perf): const should live at a shared layer both
        // crates use instead of being duplicated here once one exists.
        // Bumped from 8 MiB (real Linux's own default `ulimit -s`) to 32 MiB: a real, reproducible
        // `STATUS_STACK_OVERFLOW` was observed for `dbus-launch` specifically (confirmed via Rust's
        // own unconditional "thread '<unknown>' has overflowed its stack" guard-page message, not
        // gated behind any litebox diagnostic flag) when it runs on a real host OS thread after
        // enough prior guest activity/relocation history has occurred on that thread -- matching
        // the same underlying "host-side call frames while emulating the guest are real, heavier
        // than the guest's own limit" cost class already fixed once for `weston --use-pixman`'s
        // main-thread case (`INITIAL_GUEST_THREAD_STACK_SIZE`, this same 8->needs-more shape) and
        // once for the `--gui` presenter thread (`PRESENTER_THREAD_STACK_SIZE`, 256 MiB). Kept at a
        // modest 32 MiB (not the presenter's 256 MiB) since this applies to EVERY guest thread, not
        // one always-present background thread -- a per-thread cost that could compound under many
        // concurrent guest threads.
        const GUEST_THREAD_STACK_SIZE: usize = 32 * 1024 * 1024;
        let ctx = ctx.clone();
        // Take (clearing) whatever guest-pid the shim declared via
        // `set_next_spawned_thread_guest_pid` for the thread about to be spawned -- read on
        // THIS, the spawning thread, since the new thread's own `thread_local!`s start out
        // completely fresh/`None` and cannot see this thread's own state. Propagated into the
        // new thread's own `CURRENT_GUEST_PID` inside `thread_start`, before it registers itself
        // via `run_with_handle` (so every `CLAIMED_RANGES` operation on the new thread, from its
        // very first one, already sees the correct owner). `None` if the shim never called it
        // for this spawn -- the new thread then falls back to its own `ThreadId`, same as before
        // this mechanism existed.
        let guest_pid = NEXT_SPAWNED_THREAD_GUEST_PID.take();
        // AGENTS.md pass 217: a `fork()`'s new child inherits the ENTIRE parent address space
        // (real Windows-side memory duplication, see `Vmem::duplicate`), but `CLAIMED_RANGES`
        // ownership was never transferred -- every range the parent claimed stayed recorded
        // under the PARENT's own `ClaimOwner` forever, even after the child's own copy of that
        // memory is exclusively its own to freely replace. Root-caused live: a forked child's
        // later `execve()` doing a `MAP_FIXED` load over its own inherited memory was
        // misidentified by `find_foreign_claim` as colliding with the PARENT's still-live
        // claim, deterministically blocking ordinary concurrent `fork()`+`execve()` usage (a
        // shell forking children in a loop) even though there is no real conflict -- the child
        // is only ever replacing memory that is, after `fork()`, exclusively its own. Snapshot
        // the parent's own claim owner HERE, on the spawning (parent) thread -- `guest_pid`
        // being `Some(_)` is this same code's own existing signal that this spawn is a
        // `fork()`-shaped new guest process, not an ordinary same-process pthread clone (which
        // correctly keeps sharing the parent's claims, since it IS the same guest process) --
        // and re-claim the parent's overlapping ranges under the new child's own identity once
        // it starts running, using the same mechanism (`NEXT_SPAWNED_THREAD_GUEST_PID`'s own
        // documented pattern: read on the parent thread, moved into the child's closure) since
        // the child thread's own `thread_local!`s start out fresh and cannot see the parent's.
        let parent_owner_for_fork = guest_pid.is_some().then(current_claim_owner);
        // Constructed HERE, on the spawning thread (which already has a valid, installed TLS
        // slot and is fully protected by `vectored_exception_handler_entry`), not inside the new
        // thread's own closure -- see `thread_start`'s doc comment for why any fault during this
        // construction (in particular `continue_context`'s `Box::default()` heap allocation) must
        // never run on the new thread before `install_tls` has had a chance to run.
        let tls_state = TlsState::new();
        // TODO: do we need to wait for the handle in the main thread?
        let _handle = std::thread::Builder::new()
            .stack_size(GUEST_THREAD_STACK_SIZE)
            .spawn(move || {
                if let Some(pid) = guest_pid {
                    CURRENT_GUEST_PID.set(Some(pid));
                }
                if let Some(parent_owner) = parent_owner_for_fork {
                    reclaim_ranges_for_fork_child(parent_owner);
                }
                thread_start(init_thread, ctx, tls_state);
            })?;

        Ok(())
    }

    fn current_thread(&self) -> Self::ThreadHandle {
        CURRENT_THREAD_HANDLE.with_borrow(|current| {
            current
                .clone()
                .expect("current thread is not managed by LiteBox")
        })
    }

    fn interrupt_thread(&self, thread: &Self::ThreadHandle) {
        CURRENT_THREAD_HANDLE.with_borrow(|current| {
            thread.interrupt(current.as_ref());
        });
    }

    #[cfg(debug_assertions)]
    fn run_test_thread<R>(f: impl FnOnce() -> R) -> R {
        // Ensure the module-wide TLS slot is allocated.
        ensure_tls_index();
        let tls = TlsState::new();
        ThreadHandle::run_with_handle(&tls, f)
    }

    fn host_debug_tid(&self) -> u64 {
        u64::from(unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() })
    }

    fn set_next_spawned_thread_guest_pid(&self, pid: i32) {
        NEXT_SPAWNED_THREAD_GUEST_PID.set(Some(pid));
    }

    fn current_guest_pid(&self) -> Option<i32> {
        // Already maintained per real OS thread for `CLAIMED_RANGES`' ownership checks, and
        // propagated onto every thread this platform spawns (see `CURRENT_GUEST_PID` and
        // `set_next_spawned_thread_guest_pid`), so every thread of one guest process -- its
        // pthreads included -- answers with that process's own pid.
        CURRENT_GUEST_PID.get()
    }

    fn with_fork_duplicate_claim_owner<R>(&self, child_pid: i32, f: impl FnOnce() -> R) -> R {
        // Save/restore THIS (the parent's) thread's own `CURRENT_GUEST_PID` around `f` --
        // `duplicate()`'s eager address-space copy runs synchronously on the parent's thread, so
        // every `claim_range`/`current_claim_owner` call it makes reads this same thread-local.
        // Temporarily pointing it at the child's own future pid (see this method's doc comment
        // on `ThreadProvider` for the full concurrent-fork collision rationale) makes the copy's
        // own claims register under the CHILD's identity instead of the parent's, so a second,
        // concurrently-forking sibling child (attributed to ITS OWN distinct future pid the same
        // way) is correctly treated as a foreign owner by `find_foreign_claim` for the whole
        // vulnerable window, rather than being silently coalesced/ignored as "the parent's own
        // memory, growing normally" by `claim_range`'s same-owner-coalescing fast path.
        let prior = CURRENT_GUEST_PID.get();
        CURRENT_GUEST_PID.set(Some(child_pid));
        let result = f();
        CURRENT_GUEST_PID.set(prior);
        result
    }
}

impl litebox::platform::TimerProvider for WindowsUserland {
    type TimerHandle = TimerHandle;
    type Signal = litebox_common_linux::signal::Signal;

    fn create_timer(
        &self,
        signal: Self::Signal,
    ) -> Result<Self::TimerHandle, litebox::platform::TimerCreationError> {
        // Capture the CALLING thread's own handle so the timer callback delivers the signal
        // back to the thread that actually armed it (see `TimerCallbackContext::target` and
        // `threadpool_timer_callback` below).
        //
        // Previously this callback picked `ACTIVE_THREADS.lock().unwrap().first().cloned()` --
        // an arbitrary managed thread, not necessarily the one that owns this timer. That was a
        // correctness gap the `ACTIVE_THREADS` doc comment already flagged ("only works when we
        // support a single process"), and it stopped being merely theoretical once multiple
        // guest "processes" (each an ordinary host thread sharing this one Windows process, see
        // `spawn_thread`) could each own their own per-process `SIGALRM`/`ITIMER_REAL` timer
        // (`Process::alarm_timer`, armed via `sys_alarm`/`sys_setitimer`): whenever that timer
        // fires, delivering its signal to the wrong thread means the intended recipient never
        // sees it (a real, silent signal-delivery bug on its own) while an unrelated guest
        // process's thread gets spuriously interrupted -- if that thread has no real pending
        // signal or exit condition to act on, `prepare_to_run_guest` just returns `ready=true`
        // again immediately, and the next `switch_to_guest` can re-enter this same interrupt
        // path before making any other forward progress, i.e. a busy-livelock shaped exactly
        // like this investigation's other FS_BASE-reset livelocks. Found by code inspection while
        // investigating a separate, since-confirmed-distinct hang (`sh -c "timeout 5 tar -tzf
        // <2-gzip-member.tar.gz>"`, ultimately root-caused to a process-exit fd-leak in
        // `close_all_fds_on_process_exit`, not this); fixed on its own merits regardless, since
        // `ACTIVE_THREADS.first()` is unconditionally wrong once more than one guest process can
        // own a timer.
        let target = CURRENT_THREAD_HANDLE
            .with_borrow(Clone::clone)
            .expect("create_timer called from a thread not managed by LiteBox");
        let ctx = Box::new(TimerCallbackContext { signal, target });

        // Create a threadpool timer with the callback registered up-front.
        // The callback fires whenever the timer is armed via
        // `SetThreadpoolTimer` and the due time elapses.
        //
        // Safety: We pass a raw pointer to `ctx` which is heap-allocated via
        // `Box` and lives as long as the `TimerHandle`. The `Drop` impl
        // cancels and waits for all in-flight callbacks before the `Box` is
        // dropped, so the pointer remains valid for every callback invocation.
        let tp_timer = unsafe {
            Win32_Threading::CreateThreadpoolTimer(
                Some(threadpool_timer_callback),
                &raw const *ctx as *mut c_void,
                std::ptr::null(),
            )
        };
        assert!(
            tp_timer != 0,
            "CreateThreadpoolTimer failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(TimerHandle {
            tp_timer,
            _ctx: ctx,
        })
    }
}

pub struct TimerHandle {
    tp_timer: Win32_Threading::PTP_TIMER,
    /// Prevent the context from being dropped while the timer is alive.
    /// The raw pointer passed to the threadpool callback points into this box.
    _ctx: Box<TimerCallbackContext>,
}

impl Drop for TimerHandle {
    fn drop(&mut self) {
        // Cancel any pending callback, wait for in-flight callbacks to
        // complete, then close the threadpool timer.
        //
        // After this sequence completes the callback will never run again, so
        // it is safe to let `self.ctx` (the `Box`) drop normally.
        unsafe {
            Win32_Threading::SetThreadpoolTimer(self.tp_timer, std::ptr::null(), 0, 0);
            Win32_Threading::WaitForThreadpoolTimerCallbacks(self.tp_timer, 1);
            Win32_Threading::CloseThreadpoolTimer(self.tp_timer);
        }
    }
}

impl litebox::platform::TimerHandle for TimerHandle {
    fn set_timer(&self, duration: core::time::Duration) {
        if duration.is_zero() {
            // A zero duration cancels the timer without firing.
            // Passing NULL as the due-time pointer tells Windows to cancel
            // the pending callback.
            unsafe {
                Win32_Threading::SetThreadpoolTimer(self.tp_timer, std::ptr::null(), 0, 0);
            }
            return;
        }

        // Due time is in 100 ns intervals; negative means relative.
        // Pack into a FILETIME for SetThreadpoolTimer.
        let due_time_100ns: i64 = {
            let intervals = duration.as_nanos() / 100;
            -(i64::try_from(intervals).unwrap_or(i64::MAX))
        };
        let due_time = FILETIME {
            dwLowDateTime: due_time_100ns.cast_unsigned().trunc(),
            dwHighDateTime: (due_time_100ns >> 32).cast_unsigned().trunc(),
        };

        // Arm the threadpool timer. The callback registered at creation
        // time will fire after `duration` elapses.
        unsafe {
            Win32_Threading::SetThreadpoolTimer(
                self.tp_timer,
                &raw const due_time,
                0, // no repeat
                0, // no window
            );
        }
    }
}

/// Context shared between the `TimerHandle` and the threadpool timer callback.
struct TimerCallbackContext {
    signal: litebox_common_linux::signal::Signal,
    /// The specific thread that armed this timer (via `create_timer`), and therefore the one
    /// this timer's signal must always be delivered to -- never an arbitrary "active" thread.
    /// See the doc comment on `TimerProvider::create_timer` for why this matters.
    target: ThreadHandle,
}

/// Threadpool timer callback registered via `CreateThreadpoolTimer`.
///
/// Delivers the signal to the specific thread that armed this timer (`ctx.target`), captured at
/// `create_timer` time -- not an arbitrary active thread. See `TimerProvider::create_timer`'s
/// doc comment for the real, reproduced livelock this fixes.
unsafe extern "system" fn threadpool_timer_callback(
    _instance: Win32_Threading::PTP_CALLBACK_INSTANCE,
    context: *mut c_void,
    _timer: Win32_Threading::PTP_TIMER,
) {
    // Safety: `context` points to the `TimerCallbackContext` owned by the
    // `TimerHandle`. The handle's `Drop` impl waits for all in-flight
    // callbacks before dropping the context, so this reference is valid.
    let ctx = unsafe { &*context.cast::<TimerCallbackContext>() };
    ctx.target.deliver_signal(ctx.signal);
}

/// Console control handler registered via `SetConsoleCtrlHandler`.
///
/// When the user presses Ctrl+C, this sets the SIGINT bit on every active
/// managed thread and interrupts them so the shim can deliver the signal.
/// Ctrl+Break similarly maps to SIGTSTP (job-control suspend): both are keyboard-driven console
/// control events with no real Windows analog to "suspend a process group", so SIGTSTP is the
/// closest Linux-shell-observable behavior a real terminal's Ctrl+Z would produce.
unsafe extern "system" fn ctrl_c_handler(ctrl_type: u32) -> i32 {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};

    let signal = match ctrl_type {
        CTRL_C_EVENT => litebox_common_linux::signal::Signal::SIGINT,
        CTRL_BREAK_EVENT => litebox_common_linux::signal::Signal::SIGTSTP,
        _ => return 0, // FALSE — let the next handler deal with it
    };

    // Previously delivered to `ACTIVE_THREADS.first()` only -- the same arbitrary-thread gap
    // already identified and fixed for `create_timer`'s `SIGALRM` delivery and
    // `console_resize_watcher_thread_body`'s `SIGWINCH` delivery a few hundred lines below (see
    // either doc comment for the full "wrong-thread signal delivery -> spurious interrupt ->
    // busy-livelock or mid-syscall corruption" explanation). Real Ctrl+C/Ctrl+Break deliver
    // SIGINT/SIGTSTP to an entire foreground process group, not one arbitrary thread of one
    // arbitrary guest process -- deliver to every active thread instead.
    let threads: alloc::vec::Vec<ThreadHandle> = ACTIVE_THREADS.lock().unwrap().iter().cloned().collect();
    for thread in threads {
        thread.deliver_signal(signal);
    }

    1 // TRUE — we handled it
}

/// Runs on a dedicated background thread for the lifetime of the process: polls the console
/// output buffer's window size and delivers SIGWINCH to an active guest thread whenever it
/// changes. See the doc comment at this thread's spawn site (`WindowsUserland::new`) for why
/// polling `GetConsoleScreenBufferInfo` is used instead of an input-event-based approach.
fn console_resize_watcher_thread_body() {
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE,
    };

    // No real console attached (e.g. fully redirected stdio): nothing to poll, exit quietly
    // rather than spin forever on a handle that will never report window-size changes.
    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if handle.is_null() || handle == Win32_Foundation::INVALID_HANDLE_VALUE {
        return;
    }

    let read_size = || -> Option<(i16, i16)> {
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { core::mem::zeroed() };
        if unsafe { GetConsoleScreenBufferInfo(handle, &raw mut info) } == 0 {
            return None;
        }
        Some((
            info.srWindow.Right - info.srWindow.Left + 1,
            info.srWindow.Bottom - info.srWindow.Top + 1,
        ))
    };

    let Some(mut last_size) = read_size() else {
        // Not actually a console handle (e.g. a redirected pipe) -- nothing to watch.
        return;
    };

    loop {
        // A short sleep, not a blocking wait: there is no Win32 wait handle that signals
        // specifically on window-size change (`WaitForSingleObject` on the console input handle
        // wakes on ANY input event, which would require also filtering/re-injecting events and
        // risks the same cooked-read race `ConsoleStdinReader`'s doc comment describes -- plain
        // polling avoids touching that handle at all). 250ms is frequent enough that a resize
        // feels immediate to a human resizing a terminal window, and cheap enough not to matter
        // against a whole guest program's runtime.
        std::thread::sleep(core::time::Duration::from_millis(250));

        let Some(size) = read_size() else {
            continue;
        };
        if size != last_size {
            last_size = size;
            // Previously delivered to `ACTIVE_THREADS.first()` -- an ARBITRARY managed thread,
            // not necessarily one that cares about a window-size change, or worse, a thread
            // belonging to a completely unrelated guest process (multiple guest "processes" are
            // each an ordinary host thread sharing this one Windows process -- see
            // `create_timer`'s own doc comment a few hundred lines above, which already
            // identified and fixed the IDENTICAL "wrong-thread signal delivery" gap for
            // `SIGALRM`/`ITIMER_REAL` timers). Spuriously interrupting a thread with no real
            // pending signal to act on is not a no-op: `prepare_to_run_guest` returns
            // `ready=true` again immediately, and the next `switch_to_guest` can re-enter this
            // same interrupt path before making any other forward progress -- a busy-livelock
            // shape, or worse, a thread interrupted mid-syscall/mid-critical-section with no
            // real signal to consume. Deliver to every active thread instead (matching real
            // Linux's own SIGWINCH-to-foreground-process-group semantics, which reaches every
            // thread of every process in that group, not one arbitrary thread of one arbitrary
            // process) -- each thread's own signal-delivery/disposition logic already handles an
            // irrelevant signal correctly (default SIGWINCH disposition is Ignore).
            let threads: alloc::vec::Vec<ThreadHandle> =
                ACTIVE_THREADS.lock().unwrap().iter().cloned().collect();
            for thread in threads {
                thread.deliver_signal(litebox_common_linux::signal::Signal::SIGWINCH);
            }
        }
    }
}

/// Track-B investigation (fork-without-exec hang): runs for the lifetime of the process on its
/// own dedicated OS thread. See `FAULT_TERMINATE_ARMED_TICK`'s doc comment for the full evidence
/// this exists to work around -- a thread that is itself mid-kernel-mode-exception-delivery for
/// an unrecovered fault cannot reliably terminate its own process, so this is a genuinely
/// different, always-idle-until-needed thread standing by specifically to do it instead, exactly
/// mirroring the EXTERNAL `Stop-Process -Force` this investigation confirmed always works
/// immediately against the same wedged PID.
fn fault_terminate_watchdog_thread_body() {
    if std::env::var_os("LITEBOX_DIAG_WATCHDOG").is_some() {
        diag_raw_print(
            b"[diag-watchdog-started] win_tid=0x",
            unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() } as usize,
            b" pid=0x",
            std::process::id() as usize,
        );
    }
    // Bounded, not a tight spin: this thread does nothing for the overwhelming majority of any
    // process's lifetime (only an unrecovered AV arms the flag at all), so a coarse poll interval
    // costs nothing while still resolving a real hang within a bounded, short, human-imperceptible
    // window once one occurs.
    const POLL_INTERVAL: core::time::Duration = core::time::Duration::from_millis(200);
    // How long the primary (in-VEH) `TerminateProcess`/`RaiseFailFastException` attempt is given
    // to succeed on its own before this watchdog steps in -- generous enough that a process which
    // terminates normally (the overwhelmingly common case when the primary attempt is NOT
    // wedged) is never raced or second-guessed, short enough that a real hang is still resolved
    // promptly rather than left indefinitely.
    const GRACE_PERIOD: core::time::Duration = core::time::Duration::from_secs(3);
    // How many consecutive poll ticks the flag has been observed armed -- a simple tick COUNT
    // rather than a real timestamp, since `FAULT_TERMINATE_ARMED_TICK` is a process-wide
    // `AtomicU64` written by the (possibly wedged) faulting thread and read here from a genuinely
    // different thread: comparing two `Instant`s captured on different threads needs no special
    // care on a single machine with one monotonic clock, but a plain poll-tick counter is simpler,
    // needs no additional shared state, and gives an equally bounded, deterministic grace period
    // (`GRACE_PERIOD / POLL_INTERVAL` ticks) without introducing a second piece of cross-thread
    // shared state alongside the flag itself.
    let mut armed_ticks_seen: u32 = 0;
    let grace_ticks = u32::try_from(GRACE_PERIOD.as_millis() / POLL_INTERVAL.as_millis())
        .expect("grace period fits in a u32 tick count");
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let armed_tick = FAULT_TERMINATE_ARMED_TICK.load(Ordering::Relaxed);
        if armed_tick == 0 {
            armed_ticks_seen = 0;
            continue;
        }
        armed_ticks_seen += 1;
        if std::env::var_os("LITEBOX_DIAG_WATCHDOG").is_some() {
            diag_raw_print(
                b"[diag-watchdog-tick] armed_tick=0x",
                armed_tick as usize,
                b" armed_ticks_seen=0x",
                armed_ticks_seen as usize,
            );
        }
        if armed_ticks_seen < grace_ticks {
            continue;
        }
        // NO same-process CPU-progress safety check here (an earlier version of this fix had
        // one, using `GetProcessTimes(GetCurrentProcess(), ...)` -- removed, live-confirmed
        // broken: it reported "progress" every single cycle even against a target independently
        // confirmed via an EXTERNAL `Get-Process` check to be genuinely wedged at a flat 0% CPU
        // throughout, so the in-process measurement itself is unreliable in exactly the
        // suspended-thread state this watchdog exists to catch -- likely because `GetProcessTimes`
        // called FROM a thread inside the same frozen process does not report the same live
        // numbers an external caller (a genuinely different process) sees). The real safety
        // margin against killing a merely-slow-not-wedged process now lives entirely in this
        // grace period's own length (3 seconds, chosen generously) plus the separate EXTERNAL
        // watchdog process (`process_fork::run_external_fault_watchdog_child`), which measures
        // CPU time via `OpenProcess` from a genuinely different process and does NOT show this
        // same self-measurement unreliability -- see that function's own CPU-delta check, which
        // remains in place and IS confirmed reliable.
        //
        // Grace period elapsed and the flag is still armed:
        // during the whole window: the primary in-VEH termination attempt did not complete on its
        // own, and this is not merely a slow process. Force it now, from this genuinely different
        // thread. `diag_raw_print` (allocation-free, syscall-only) rather than `eprintln!`,
        // matching this file's own established convention for a print that must survive even if
        // the process is in a degraded state by the time this runs.
        diag_raw_print(
            b"[diag-fault-watchdog-terminate] armed_tick=0x",
            armed_tick as usize,
            b" poll_ticks_waited=0x",
            armed_ticks_seen as usize,
        );
        unsafe {
            windows_sys::Win32::System::Threading::TerminateProcess(
                windows_sys::Win32::System::Threading::GetCurrentProcess(),
                1,
            );
        }
        // If `TerminateProcess` itself somehow does not immediately end this thread too (it
        // should), fall back to a hard `std::process::exit`, and loop back around to keep trying
        // rather than let the watchdog itself silently stop covering a still-wedged process.
        std::process::exit(1);
    }
}

/// Helper to lock two mutexes in address order, to prevent deadlock. Shared by
/// `ThreadHandle::interrupt` and `ctxwatch_arm_other_threads`, both of which suspend a target
/// thread from a "current" thread and must avoid two threads each locking the other's mutex in
/// opposite order concurrently.
fn lock_two<'a, T, U>(
    left: &'a Mutex<T>,
    right: &'a Mutex<U>,
) -> (std::sync::MutexGuard<'a, T>, std::sync::MutexGuard<'a, U>) {
    if std::ptr::from_ref(left).addr() < std::ptr::from_ref(right).addr() {
        let l = left.lock().unwrap();
        let r = right.lock().unwrap();
        (l, r)
    } else {
        let r = right.lock().unwrap();
        let l = left.lock().unwrap();
        (l, r)
    }
}

/// Diagnostic-only (`LITEBOX_CTXWATCH=1`): arms the same `ctx.rip` write-watchpoint `ctxwatch::arm`
/// just set on the calling thread on every OTHER live thread registered in `ACTIVE_THREADS` too.
/// Debug registers are per-thread Windows state (virtualized via `Get`/`SetThreadContext`), so a
/// watchpoint armed on only one thread can never catch a write made by an instruction executing
/// on a different thread -- this closes that coverage gap by reusing the same
/// suspend/get-context/set-context/resume sequence `ThreadHandle::interrupt` already relies on for
/// cross-thread context manipulation elsewhere in this file. Best-effort and non-fatal: a thread
/// that disappears or fails to arm is skipped, logged, and does not stop the rest.
///
/// Lock ordering mirrors `ThreadHandle::interrupt` exactly: `current`'s own mutex is held for the
/// duration of each per-target suspend/arm/resume, via the same `lock_two` address-ordered
/// two-mutex acquisition `interrupt` uses. This matters because, unlike a diagnostic that only
/// ever touches one target, multiple pipeline threads can each independently reach this same
/// exit_group path and call this function concurrently -- without holding its own lock while
/// suspending another thread, thread A suspending B while B concurrently (and lock-free) suspends
/// A is exactly the ABBA deadlock/race `interrupt`'s `lock_two` pattern was written to prevent.
fn ctxwatch_arm_other_threads(ctx: *const litebox_common_linux::PtRegs) {
    let addr = (ctx as usize).wrapping_add(ctxwatch::RIP_FIELD_OFFSET);
    let Some(current) = CURRENT_THREAD_HANDLE.with(|c| c.borrow().clone()) else {
        // Not a LiteBox-managed thread; nothing to lock ourselves against.
        return;
    };
    let others: alloc::vec::Vec<ThreadHandle> = ACTIVE_THREADS
        .lock()
        .unwrap()
        .iter()
        .filter(|h| !Arc::ptr_eq(&current.0, &h.0))
        .cloned()
        .collect();
    for other in others {
        // Lock both `current` and `other` (address-ordered, matching `interrupt`) so this thread
        // is never suspended by a concurrent caller while it holds `other`'s lock, and vice versa.
        let (_current_guard, guard) = lock_two(&current.0, &other.0);
        let Some(inner) = guard.as_ref() else {
            continue;
        };
        let raw_handle = inner.handle.as_raw_handle();
        // SAFETY: `raw_handle` comes from a live `ThreadHandleInner` held under its own lock, so
        // the OS thread handle is valid for the duration of this suspend/arm/resume sequence.
        unsafe {
            windows_sys::Win32::System::Threading::SuspendThread(raw_handle);
        }
        let _resume_guard = litebox::utils::defer(|| unsafe {
            windows_sys::Win32::System::Threading::ResumeThread(raw_handle);
        });
        // SAFETY: `raw_handle` is a valid, now-suspended thread handle.
        unsafe {
            ctxwatch::arm_on_handle(raw_handle, addr);
        }
    }
}

#[derive(Clone)]
pub struct ThreadHandle(Arc<Mutex<Option<ThreadHandleInner>>>);

struct ThreadHandleInner {
    handle: std::os::windows::io::OwnedHandle,
    tls: SendConstPtr<TlsState>,
}

struct SendConstPtr<T>(*const T);
unsafe impl<T> Send for SendConstPtr<T> {}

thread_local! {
    static CURRENT_THREAD_HANDLE: RefCell<Option<ThreadHandle>> = const { RefCell::new(None) };
}

thread_local! {
    /// This thread's own guest-space process id, if one has ever been assigned -- see
    /// [`ThreadProvider::set_next_spawned_thread_guest_pid`]'s doc comment for why this exists
    /// and how it propagates from a spawning thread to the thread it spawns.
    ///
    /// `None` on a thread that never went through this propagation (the very first/initial
    /// guest thread of a fresh `litebox_runner` invocation, or a host-only test thread) -- such
    /// a thread's own `std::thread::ThreadId` is already a correct, unique-enough proxy for its
    /// guest-process identity on its own, since it has no sibling thread within the same guest
    /// process to be confused with.
    static CURRENT_GUEST_PID: Cell<Option<i32>> = const { Cell::new(None) };
}

thread_local! {
    /// Set by [`WindowsUserland::set_next_spawned_thread_guest_pid`] on the SPAWNING thread,
    /// immediately before its own call to [`ThreadProvider::spawn_thread`]; read and cleared by
    /// that same call (on the SAME, spawning thread, never the new one) to capture the value to
    /// propagate into the new thread's own [`CURRENT_GUEST_PID`].
    static NEXT_SPAWNED_THREAD_GUEST_PID: Cell<Option<i32>> = const { Cell::new(None) };
}

/// Returns the calling thread's own guest-process identity for [`CLAIMED_RANGES`] ownership
/// purposes: its propagated [`CURRENT_GUEST_PID`] if one was ever assigned (recognizing sibling
/// pthreads of the SAME guest process as the SAME owner), falling back to the real
/// [`std::thread::ThreadId`] for a thread that never went through that propagation (see
/// [`CURRENT_GUEST_PID`]'s doc comment).
fn current_claim_owner() -> ClaimOwner {
    match CURRENT_GUEST_PID.get() {
        Some(pid) => ClaimOwner::GuestPid(pid),
        None => ClaimOwner::ThreadId(std::thread::current().id()),
    }
}

/// Global registry of all active managed thread handles.
///
/// Threads are registered in [`ThreadHandle::run_with_handle`] and
/// removed when the guard drops.
///
/// TODO: This global list only works when we support a single process. For
/// multi-process support, each process (or `WindowsUserland` instance) should
/// track its own thread list.
static ACTIVE_THREADS: Mutex<alloc::vec::Vec<ThreadHandle>> = Mutex::new(alloc::vec::Vec::new());

/// The owner of a [`ClaimSlot`]: either a real host [`std::thread::ThreadId`] (a thread that
/// never had a guest-pid propagated onto it, see [`CURRENT_GUEST_PID`]) or a guest-space process
/// id shared by every one of that guest process's own OS threads (see
/// `ThreadProvider::set_next_spawned_thread_guest_pid`'s doc comment) -- two claims sharing the
/// SAME guest pid are the SAME guest process's own memory, even when their real OS `ThreadId`s
/// differ (e.g. a guest process's own additional pthread, not a `fork()`ed sibling process).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClaimOwner {
    ThreadId(std::thread::ThreadId),
    GuestPid(i32),
}

/// One entry in [`CLAIMED_RANGES`]: a host address range, the [`ClaimOwner`] of the guest
/// process that owns it (for collision/coalescing checks -- shared across every OS thread of the
/// same guest process), the real [`std::thread::ThreadId`] of the SPECIFIC thread that inserted
/// it (for release-on-thread-exit, which must only drop THIS thread's own entries, never a
/// still-live sibling thread's -- see `release_all_claims_for_current_thread`'s doc comment), and
/// a monotonically-increasing insertion sequence number used to find the single oldest entry for
/// eviction when the registry is full (see `claim_range`). `None` means the slot is empty.
type ClaimSlot = Option<(core::ops::Range<usize>, ClaimOwner, std::thread::ThreadId, u64)>;

/// How many concurrently-live guest processes can each hold a `Replace`-mode claim at once (see
/// [`CLAIMED_RANGES`]'s doc comment). A fixed, generous upper bound rather than a growable
/// collection deliberately: `BTreeMap`/`Vec::push` route through this process's own
/// `#[global_allocator]` (`SafeZoneAllocator`, a `spin::SpinMutex`-protected slab/buddy
/// allocator, `litebox/src/mm/allocator.rs`), and calling into it from `allocate_pages`'s already
/// timing-sensitive `Replace` path was found live to make an existing, pre-existing, still-not-
/// fully-understood class of Windows scheduling/segment-MSR instability (see the FS_BASE/GS_BASE
/// repair sites above) fire far more often -- a fixed-size array sidesteps the allocator (and
/// its spinlock) entirely for this registry's own bookkeeping.
///
/// Originally 64, sized only for "a handful of nested `vfork()` levels" -- confirmed live via a
/// weston repro to be a real under-count once `Hint`-mode calls started reaching `claim_range`
/// (see that function's own doc comment): a single guest process doing ordinary DRM/GL/dynamic-
/// library `mmap(NULL,...)` churn produces hundreds of small, mutually non-adjacent ranges in
/// well under a second (633 real, logged `claim_range DROPPED (registry full)` events observed
/// in one 30-second repro), silently evicting a DIFFERENT, still-live guest process's own
/// legitimate claim -- exactly the collision this registry exists to prevent. Raised to 512
/// (8x): a full XFCE desktop session (weston + xfsettingsd + xfce4-panel + xfdesktop + xfconfd +
/// dbus-daemon + at-spi-bus-launcher, ~20 real OS threads, each doing its own concurrent dynamic-
/// library-loading churn during startup) was confirmed live to exhaust the 512-slot registry
/// within ~20 seconds of the first few clients launching (844 real eviction events logged in one
/// repro, `occupied=512 max=512` sustained thereafter) -- and unlike the single-process case 512
/// was tuned for, evictions here landed on STILL-LIVE, actively-loading sibling processes
/// (`xfsettingsd`/`xfdesktop`, confirmed via the evicted entries' own logged `GuestPid` owners),
/// not stale leftovers. Every one of those processes' every thread then permanently stalled in a
/// genuine (non-corrupted, `cdb`-confirmed) `WaitOnAddress` a few seconds later with no wake ever
/// arriving -- consistent with this doc comment's own described failure mode (a later
/// `Replace`-mode allocation silently decommitting/recommitting straight over the evicted range's
/// still-live memory, corrupting a live thread's own state with no crash, no page fault, and no
/// guest-visible signal, only an unexplained later hang).
///
/// First tried raising this to 4096 (8x again) -- confirmed live to be the WRONG fix on its own:
/// both `claim_range`'s mandatory per-call coalescing scan and (before it was removed, see
/// `find_foreign_claim`'s own doc comment) an unconditional debug-log-only occupancy count scaled
/// linearly with `MAX_CLAIMS`, and at 4096 slots this cost enough extra latency across the ~9000
/// `claim_range`/`find_foreign_claim` calls a full XFCE session's startup churn produces that
/// weston's own timing-sensitive DRM initialization sequence never completed a single
/// `DRM_IOCTL_MODE_SETCRTC` in a 108+ second repro (previously ~15-25s) -- trading the original
/// hang for an even worse one. Settled on 2048 (4x, half the scan cost of the 4096 attempt) after
/// also removing `find_foreign_claim`'s superfluous full-array occupancy scan (used only to
/// populate a debug log field, paid unconditionally regardless of whether logging was even
/// enabled) -- combined, these give real headroom over the ~900-1000 peak occupancy observed
/// before eviction previously kicked in, without reintroducing the 4096 attempt's own regression.
/// Genuine LRU eviction (below) remains the correctness backstop for whatever churn volume still
/// exceeds 2048: exhaustion now evicts the single OLDEST entry (by insertion sequence, tracked in
/// `ClaimSlot`) rather than silently dropping the NEWEST one -- the newest claim is, by
/// construction, the one about to be relevant to an imminent collision check, while an entry old
/// enough to be the least-recently-inserted across the WHOLE registry is far more likely to
/// belong to memory that's since been superseded or released. This only gives up this registry's
/// own collision defense for whichever single entry loses the eviction race, never correctness
/// of anything else.
const MAX_CLAIMS: usize = 2048;

/// Host address ranges currently claimed by a live guest "process" (a real OS thread), see
/// [`ClaimSlot`]/[`MAX_CLAIMS`] for the storage shape and why it is a fixed array.
///
/// # Why this exists
///
/// Every guest "process" in this architecture is a real OS thread sharing ONE real Windows
/// process (see this module's top-level doc comment) -- there is no per-process address space
/// isolation the way real Linux `fork()`/`execve()` gets for free. `Vmem::insert_mapping`'s
/// `FixedAddressBehavior::Replace` path (real `MAP_FIXED`, used for every ELF segment of a
/// non-PIE/`ET_EXEC` binary, which loads at a fixed, non-negotiable address baked into the ELF
/// itself -- `gcc`, `cc1`, and most Alpine/musl binaries) can only see ITS OWN guest-level
/// `Vmem` bookkeeping plus the platform's raw `VirtualQuery` state; neither can distinguish "this
/// committed range is a stale leftover from MY OWN prior `execve()` on this same thread, safe to
/// overwrite" from "this committed range is another guest process's CURRENTLY LIVE memory" --
/// e.g. a `vfork()`-blocked parent's own image, still fully intact and about to resume. Two
/// non-PIE binaries loaded at the same address (extremely common: EVERY `ET_EXEC` binary with
/// the same link-time base, e.g. `0x400000`, collides with every other) that happen to be alive
/// at the same real moment -- a `vfork()`-ing child that itself `vfork()`s again, e.g. `gcc`
/// (still blocked, waiting on its own child) `vfork()`ing `cc1` -- silently clobber each other's
/// real memory with no error, no page fault, and no guest-visible signal: confirmed live via a
/// Windows minidump showing a `ret` faulting on a `rsp` that no longer pointed at valid committed
/// memory, because a sibling process's fixed-address ELF load had silently decommitted and
/// recommitted straight over top of it.
///
/// This registry closes that gap: every successful [`WindowsUserland::allocate_pages`] call whose
/// `fixed_address_behavior` is [`FixedAddressBehavior::Replace`] (the only mode with no existing
/// collision defense -- `Hint`/`NoReplace` already refuse to build on a real `MEM_COMMIT` via
/// `has_committed_page`) records its range here under the CALLING thread's own
/// [`std::thread::ThreadId`] (stable for a guest process's entire lifetime -- `execve` reuses the
/// same real OS thread, never spawning a new one). Deliberately consulted and updated ONLY on
/// this already-rare, already-`VirtualQuery`-scanning `Replace` path -- NOT on every
/// `allocate_pages` call -- so ordinary `Hint`-mode allocation (guest heap/stack/mmap growth, the
/// overwhelming majority of calls) pays no additional cost at all.
static CLAIMED_RANGES: Mutex<[ClaimSlot; MAX_CLAIMS]> = Mutex::new([const { None }; MAX_CLAIMS]);

/// Monotonically-increasing insertion counter for [`ClaimSlot`]'s sequence field, guarded by the
/// same [`CLAIMED_RANGES`] lock (never accessed independently) -- used only to find the single
/// oldest entry when the registry is full and a new claim needs a slot (see `claim_range`).
static NEXT_CLAIM_SEQ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// High-water mark of [`CLAIMED_RANGES`] occupancy, and the last value already reported.
///
/// Exhausting the registry is not a tidiness problem: eviction drops a claim that may still
/// describe LIVE memory, which is the exact cross-process collision this registry exists to
/// prevent (see [`MAX_CLAIMS`], whose own doc comment records two separate live sessions killed
/// this way). The tuning history there was driven by after-the-fact reasoning about repro logs
/// because nothing ever reported how full the registry actually got. These do, at zero added
/// cost: [`claim_range`] already walks the whole array for coalescing, so the count folds into a
/// scan that was happening anyway rather than adding one.
static CLAIM_HIGH_WATER: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// Last high-water value actually logged, so a rising mark reports in coarse steps instead of on
/// every single claim -- this is `allocate_pages`'s hot path and the `MAX_CLAIMS` doc comment
/// records log volume alone regressing a live weston session.
static CLAIM_HIGH_WATER_REPORTED: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
/// Granularity of the high-water report above.
const CLAIM_HIGH_WATER_STEP: usize = 128;

/// Returns the (range, owner) of any claimed range overlapping `range` whose owner is NOT
/// `exclude_owner`, if one exists.
///
/// Deliberately does not also compute/log an occupancy count here -- an earlier version did, via
/// an unconditional `claims.iter().filter(...).count()` purely to populate a debug-log field,
/// paid on every call regardless of whether logging was even enabled. At `MAX_CLAIMS`'s current
/// size that extra full-array scan, multiplied across the thousands of calls a real XFCE
/// session's startup churn produces, was confirmed live to contribute real, measurable latency to
/// this already-hot path -- see [`MAX_CLAIMS`]'s own doc comment for the full story.
fn find_foreign_claim(
    range: core::ops::Range<usize>,
    exclude_owner: ClaimOwner,
) -> Option<(core::ops::Range<usize>, ClaimOwner)> {
    // AGENTS.md pass 213/214: `exclude_owner` is a snapshot of `current_claim_owner()` taken at
    // THIS call site, which returns `ClaimOwner::GuestPid(pid)` once `CURRENT_GUEST_PID` has been
    // set for this thread and `ClaimOwner::ThreadId(...)` before that -- but a single guest
    // process's OWN earlier claim (e.g. from a prior segment of the SAME `execve`'s multi-segment
    // ELF load, or from before `CURRENT_GUEST_PID` was propagated onto this thread) can have been
    // recorded under the OTHER variant. Comparing only `*owner != exclude_owner` then
    // misidentifies a thread's own prior claim as "foreign", when what this check actually needs
    // to know is "does this range belong to a DIFFERENT real OS thread" -- root-caused live: after
    // pass 213's mmap-address-verification fix started correctly REJECTING (rather than silently
    // corrupting on) exactly this kind of false-positive foreign-claim hit, EVERY `execve`
    // (not just the historically-observed 8th) began failing with `EEXIST`, including trivial
    // single-segment binaries (`/bin/mkdir`, `/bin/chmod`, `/bin/sleep`) with no plausible genuine
    // cross-process collision. The real, always-stable identity for "is this my own thread's
    // memory" is the real host `ThreadId` stored alongside each claim (`_tid`, previously unused
    // for exclusion) -- always consistent across a single thread's lifetime including every
    // `execve` on it, unlike `ClaimOwner`, which can legitimately change mid-lifetime.
    let this_thread = std::thread::current().id();
    let claims = CLAIMED_RANGES.lock().unwrap();
    claims.iter().find_map(|slot| {
        slot.as_ref().and_then(|(claimed, owner, tid, _seq)| {
            (*owner != exclude_owner
                && *tid != this_thread
                && claimed.start < range.end
                && claimed.end > range.start)
                .then(|| (claimed.clone(), *owner))
        })
    })
}

/// Real host address ranges backing a live guest OS thread's own Windows stack reservation
/// (`[DeallocationStack, StackBase)`, read once via the TEB at thread start -- see
/// [`register_current_thread_stack`]).
///
/// [`CLAIMED_RANGES`] only ever records ranges that went through
/// [`WindowsUserland::allocate_pages`] itself (guest heap/mmap growth); a thread's own real
/// stack is placed directly by Windows at `std::thread::Builder::spawn` time and NEVER goes
/// through `allocate_pages`, so it was never a "claim" `find_foreign_claim` could see. Confirmed
/// live via the `gcc`/`ET_EXEC` compile crash this registry fixes: a non-PIE binary's fixed,
/// non-negotiable ELF load address (e.g. `0x400000`) landing inside a DIFFERENT, currently-live
/// guest thread's own real stack -- `allocate_pages`'s `Replace`-mode path saw `find_foreign_claim`
/// return `None` (correctly -- nothing had "claimed" that address) and fell through to
/// decommit-and-recommit directly over the other thread's live stack memory, corrupting it with
/// no page fault or guest-visible signal. A small `Vec`, not a fixed array like `CLAIMED_RANGES`:
/// entry count is bounded by the number of concurrently-live real OS threads (tens, not the
/// thousands of heap/mmap claims `CLAIMED_RANGES` churns through), so the `MAX_CLAIMS`-tuning
/// latency history that governs the fixed-array design there does not apply here.
static LIVE_THREAD_STACKS: Mutex<alloc::vec::Vec<(core::ops::Range<usize>, std::thread::ThreadId)>> =
    Mutex::new(alloc::vec::Vec::new());

/// Records the CALLING thread's own real stack reservation into [`LIVE_THREAD_STACKS`], read via
/// the TEB (`gs:[0x1478]` = `DeallocationStack`, the true bottom of the whole reservation;
/// `gs:[0x08]` = `StackBase`, the top) -- the same offsets already used for the
/// `LITEBOX_VEH_TRACE=1` `DIAG-REALSTACK` diagnostic elsewhere in this file. Must be called once,
/// early, on every newly spawned guest OS thread (both `spawn_thread`'s `clone()`-spawned threads
/// and the initial/root guest thread in the runner crate) -- BEFORE that thread ever runs guest
/// code that could trigger a colliding `Replace`-mode `allocate_pages` call from ANOTHER thread.
fn register_current_thread_stack() {
    let stack_base: u64;
    let dealloc_stack: u64;
    unsafe {
        core::arch::asm!(
            "mov {0}, gs:[0x08]",
            "mov {1}, gs:[0x1478]",
            out(reg) stack_base,
            out(reg) dealloc_stack,
        );
    }
    let range = (dealloc_stack as usize)..(stack_base as usize);
    if range.is_empty() {
        return;
    }
    let tid = std::thread::current().id();
    litebox_util_log::debug!(
        start:% = range.start, end:% = range.end, tid:? = tid;
        "register_current_thread_stack"
    );
    LIVE_THREAD_STACKS.lock().unwrap().push((range, tid));
}

/// Removes every range this thread registered via [`register_current_thread_stack`]. Called once,
/// alongside [`unclaim_thread`], when a guest process's real OS thread is about to exit -- a dead
/// thread's stack is no longer live memory and must stop being reported as a foreign collision.
fn unregister_current_thread_stack() {
    let tid = std::thread::current().id();
    LIVE_THREAD_STACKS.lock().unwrap().retain(|(_, t)| *t != tid);
}

/// Returns the range of any OTHER live thread's own real stack reservation overlapping `range`,
/// if one exists. See [`LIVE_THREAD_STACKS`]'s doc comment for why this check exists alongside,
/// not instead of, [`find_foreign_claim`].
fn find_live_stack_overlap(range: core::ops::Range<usize>) -> Option<core::ops::Range<usize>> {
    let self_tid = std::thread::current().id();
    let stacks = LIVE_THREAD_STACKS.lock().unwrap();
    stacks.iter().find_map(|(stack, tid)| {
        (*tid != self_tid && stack.start < range.end && stack.end > range.start)
            .then(|| stack.clone())
    })
}

/// Records that the calling thread now owns `range`, coalescing it into any of ITS OWN prior
/// entries that overlap OR are immediately adjacent to it (a re-`execve` or a `Replace` over
/// one's own stale leftover legitimately changes what this thread owns at that address; ordinary
/// contiguous heap/mmap growth on the SAME thread should extend one bounding entry rather than
/// consume a fresh slot per call) but leaving every other thread's entries untouched.
///
/// Originally called only for `Replace`-mode allocations; now also called for `Hint`-mode ones
/// (see `allocate_pages`'s own call sites) -- a `Hint`-mode allocation (e.g. an ordinary guest
/// `mmap(NULL, ...)`) commits real, live host memory just as much as a `Replace`-mode one does,
/// and a DIFFERENT thread's LATER `Replace`-mode fixed-address allocation (e.g. that thread's own
/// `brk()` growth) can land on this exact real address with no page fault or guest-visible signal
/// if this range was never claimed -- confirmed live via a weston + weston-desktop-shell repro
/// where the child's freshly-`brk()`'d heap landed exactly on the parent's own live, unclaimed
/// `mmap(NULL, 4096)` region, and `find_foreign_claim` returning `None` for it let the `Replace`
/// path decommit-and-recommit straight over the parent's still-live memory. The merge-into-one-
/// bounding-entry behavior (rather than the strict supersede-only behavior this function used
/// before `Hint` calls started reaching it) keeps the fixed-size registry from being exhausted by
/// ordinary, frequent, mostly-contiguous heap/mmap growth, which `Replace`-only callers never
/// produced enough of to matter.
///
/// Silently drops the claim if every slot is already in use by some OTHER thread's own ranges
/// (see [`MAX_CLAIMS`] -- this only gives up this registry's own collision defense for the
/// dropped claim, never a hard error).
fn claim_range(range: core::ops::Range<usize>) {
    if range.is_empty() {
        return;
    }
    let owner = current_claim_owner();
    let tid = std::thread::current().id();
    litebox_util_log::debug!(
        start:% = range.start, end:% = range.end, owner:? = owner;
        "allocate_pages: DIAG claim_range"
    );
    let mut claims = CLAIMED_RANGES.lock().unwrap();
    // Absorb this GUEST PROCESS's own prior entries (matched by `ClaimOwner`, shared across
    // every one of its own OS threads -- not just this specific thread's own `ThreadId`) that
    // overlap OR touch (are immediately adjacent to) the new range into one merged bound, rather
    // than dropping them outright -- this is what keeps ordinary sequential heap/mmap growth on
    // one thread from consuming a fresh slot per call, and now ALSO coalesces a sibling
    // pthread's own overlapping claim into the same guest process's own bound.
    let mut merged = range;
    // Occupancy is counted in this same pass rather than by a second scan: see
    // `CLAIM_HIGH_WATER`. Counted AFTER the coalescing clear below, so it reflects what the
    // registry actually holds going into the insert.
    let mut occupied = 0usize;
    for slot in claims.iter_mut() {
        if let Some((claimed, o, _tid, _seq)) = slot
            && *o == owner
            && claimed.start <= merged.end
            && claimed.end >= merged.start
        {
            merged.start = merged.start.min(claimed.start);
            merged.end = merged.end.max(claimed.end);
            *slot = None;
        }
        if slot.is_some() {
            occupied += 1;
        }
    }
    // `+ 1` for the entry about to be inserted (or, when full, to replace an evicted one).
    let occupancy = occupied + 1;
    if CLAIM_HIGH_WATER.fetch_max(occupancy, core::sync::atomic::Ordering::Relaxed) < occupancy {
        let reported = CLAIM_HIGH_WATER_REPORTED.load(core::sync::atomic::Ordering::Relaxed);
        if occupancy >= reported.saturating_add(CLAIM_HIGH_WATER_STEP) {
            CLAIM_HIGH_WATER_REPORTED.store(occupancy, core::sync::atomic::Ordering::Relaxed);
            litebox_util_log::warn!(
                occupancy:% = occupancy, max:% = MAX_CLAIMS;
                "claim_range: CLAIMED_RANGES high-water mark rose"
            );
        }
    }
    let seq = NEXT_CLAIM_SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if let Some(empty_slot) = claims.iter_mut().find(|s| s.is_none()) {
        *empty_slot = Some((merged, owner, tid, seq));
    } else {
        // Registry genuinely full even after this thread's own coalescing -- evict the single
        // OLDEST entry (lowest sequence number) across the WHOLE registry, regardless of owner,
        // rather than silently dropping this brand-new claim. See `MAX_CLAIMS`'s doc comment for
        // why the newest claim is the one worth keeping when a choice must be made.
        let oldest_idx = claims
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|(_, _, _tid, seq)| (i, *seq)))
            .min_by_key(|(_, seq)| *seq)
            .map(|(i, _)| i);
        if let Some(idx) = oldest_idx {
            // `warn!`, not `debug!`. This drops a claim that may still describe LIVE memory of
            // a still-running guest process, re-opening the silent cross-process corruption this
            // registry exists to close -- `MAX_CLAIMS`'s doc comment records two separate live
            // desktop sessions killed exactly this way. It was previously invisible at the
            // default log level, so the only evidence it had happened was the hang it caused
            // seconds later.
            litebox_util_log::warn!(
                start:% = merged.start, end:% = merged.end, owner:? = owner,
                max:% = MAX_CLAIMS;
                "claim_range: registry full, evicting oldest entry (a live claim may be lost)"
            );
            claims[idx] = Some((merged, owner, tid, seq));
        }
    }
}

/// Drops the calling guest process's claim over `range`, because that host memory has just been
/// genuinely released back to Windows.
///
/// # Why this must exist
///
/// [`CLAIMED_RANGES`] previously only ever GREW for a given owner: [`claim_range`] coalesces each
/// new claim with the same owner's overlapping-or-adjacent entries into one merged bound, and the
/// only removal path was [`release_all_claims_for_current_thread`], which runs when an entire OS
/// THREAD exits. Nothing dropped a claim when the memory under it was unmapped. A long-lived
/// process that maps and unmaps repeatedly therefore accumulated one ever-widening claim covering
/// address space it no longer owned -- and Windows, which does know the memory was freed, is free
/// to hand that same address to a DIFFERENT guest process.
///
/// The consequence was fatal and looked nothing like its cause. `ld.so` maps a shared library in
/// two steps: `mmap(NULL, whole_span, PROT_READ)` to reserve it, then `mmap(base + off, len,
/// MAP_FIXED)` for each segment INSIDE that reservation. Step one succeeded; step two arrived as
/// a `Replace`-mode fixed request, hit a stale foreign claim over its own freshly-reserved range,
/// and was relocated to an OS-chosen address (`base_addr = null`). `do_mmap`'s post-check then
/// correctly refused a `MAP_FIXED` that did not land where it was asked, returning `EEXIST`, and
/// glibc reported `libc.so.6: failed to map segment from shared object`. Every process started
/// after a heavy mapper (selkies, a Python process) intermittently failed to start at all; that
/// is what kept `dbus-daemon` -- and therefore the whole XFCE session -- from ever coming up.
///
/// Removing rather than splitting on a punch-out is deliberate. The two error directions are not
/// symmetric: a MISSING claim only weakens a collision heuristic, while a claim that outlives its
/// memory actively breaks correct programs. When a freed range falls strictly inside a claim,
/// this keeps only the part below it rather than consuming a second slot to represent the hole.
/// Whether Windows itself reports ANY reserved or committed memory inside `range`.
///
/// The claim registry is bookkeeping; Windows is the ground truth for whether memory exists. A
/// claim covering a range Windows says is entirely `MEM_FREE` is provably stale -- the memory it
/// described has already been released -- and honouring it corrupts nothing but does break the
/// caller, so the registry must lose that argument.
fn range_holds_real_memory(range: &core::ops::Range<usize>) -> bool {
    let mut address = range.start;
    while address < range.end {
        let mut mbi = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
        let ok = unsafe {
            Win32_Memory::VirtualQuery(
                address as *const c_void,
                &raw mut mbi,
                core::mem::size_of::<Win32_Memory::MEMORY_BASIC_INFORMATION>(),
            ) != 0
        };
        if !ok {
            // Cannot tell -- assume it is real, i.e. keep the conservative old behaviour.
            return true;
        }
        if mbi.State == Win32_Memory::MEM_RESERVE || mbi.State == Win32_Memory::MEM_COMMIT {
            return true;
        }
        let next = (mbi.BaseAddress as usize).saturating_add(mbi.RegionSize);
        if next <= address {
            return true;
        }
        address = next;
    }
    false
}

/// Drops EVERY owner's claim over `range`, used when Windows has proven the range holds no real
/// memory (see [`range_holds_real_memory`]). Unlike [`unclaim_range`] this deliberately ignores
/// ownership: a stale entry belongs to whichever process last held the address, never to the one
/// discovering it.
fn purge_stale_claims(range: &core::ops::Range<usize>) {
    for slot in CLAIMED_RANGES.lock().unwrap().iter_mut() {
        if let Some((claimed, ..)) = slot
            && claimed.start < range.end
            && claimed.end > range.start
        {
            *slot = None;
        }
    }
}

fn unclaim_range(range: core::ops::Range<usize>) {
    if range.is_empty() {
        return;
    }
    let owner = current_claim_owner();
    // NEVER block to release a claim.
    //
    // This runs from `Vmem::remove_mapping`, i.e. underneath the caller's own memory-manager
    // lock, on every guest unmap. Taking `CLAIMED_RANGES` blocking there froze the entire guest:
    // with this call enabled the whole runtime went silent ~99 s into an XFCE session (every
    // thread, not just the unmapping one) while the identical run with it disabled ran past
    // 250 s. The registry is a HEURISTIC that exists to spot cross-process collisions -- failing
    // to record a release only weakens that heuristic, and the consumer already re-validates a
    // claim against Windows before acting on it (`range_holds_real_memory`), so a missed release
    // is self-correcting. Deadlocking the guest is not. Bounded spin, then give up.
    let mut guard = None;
    for _ in 0..256 {
        if let Ok(g) = CLAIMED_RANGES.try_lock() {
            guard = Some(g);
            break;
        }
        core::hint::spin_loop();
    }
    let Some(mut claims) = guard else {
        return;
    };
    for slot in claims.iter_mut() {
        let Some((claimed, o, _tid, _seq)) = slot else {
            continue;
        };
        // Only this guest process's own claims: another owner's entry overlapping memory this
        // process is freeing would mean the bookkeeping was already wrong, and silently dropping
        // a live process's claim is exactly the failure this registry exists to prevent.
        if *o != owner || claimed.end <= range.start || claimed.start >= range.end {
            continue;
        }
        if range.start <= claimed.start && range.end >= claimed.end {
            *slot = None;
            continue;
        }
        if range.start <= claimed.start {
            claimed.start = range.end;
        } else {
            claimed.end = range.start;
        }
        if claimed.start >= claimed.end {
            *slot = None;
        }
    }
}

/// Re-claims every one of `parent_owner`'s ranges under the CALLING thread's own
/// [`current_claim_owner`] (the new `fork()` child, whose `CURRENT_GUEST_PID` must already be
/// set before calling this). See the call site in [`WindowsUserland::spawn_thread`] for the
/// full rationale (AGENTS.md pass 217): a `fork()` child inherits its parent's entire address
/// space, but never inherited the parent's `CLAIMED_RANGES` ownership until this function --
/// without it, the child's own later `MAP_FIXED` replacement of its own inherited memory was
/// misidentified as colliding with the (unrelated, still-live) parent.
///
/// Snapshots the matching entries first, then inserts the copies via the ordinary
/// [`claim_range`] (which itself coalesces adjacent/overlapping same-owner entries) -- never
/// removes the parent's own original entries, since the parent's own memory reservation is
/// still real and still needs its own collision defense against OTHER unrelated processes.
fn reclaim_ranges_for_fork_child(parent_owner: ClaimOwner) {
    let matching: alloc::vec::Vec<core::ops::Range<usize>> = {
        let claims = CLAIMED_RANGES.lock().unwrap();
        claims
            .iter()
            .filter_map(|slot| {
                slot.as_ref()
                    .and_then(|(range, owner, _tid, _seq)| (*owner == parent_owner).then(|| range.clone()))
            })
            .collect()
    };
    for range in matching {
        claim_range(range);
    }
}

/// Removes every range inserted BY THE CALLING THREAD ITSELF. Called once, from
/// [`ThreadHandle::run_with_handle`]'s teardown guard, when a guest process's real OS thread
/// itself exits -- see [`CLAIMED_RANGES`]'s doc comment for why `execve` alone must not do this.
///
/// Deliberately matches on the real [`std::thread::ThreadId`] that INSERTED each entry, never
/// the (possibly-shared-across-threads) [`ClaimOwner`] -- a guest process's own additional
/// pthread exiting must only release THAT THREAD's own claims, never a still-live sibling
/// thread's (e.g. the SAME guest process's main thread) claims that happen to share the same
/// `ClaimOwner::GuestPid`.
fn release_all_claims_for_current_thread() {
    let tid = std::thread::current().id();
    for slot in CLAIMED_RANGES.lock().unwrap().iter_mut() {
        if matches!(slot, Some((_, _owner, t, _seq)) if *t == tid) {
            *slot = None;
        }
    }
}

/// Best-effort check: is `rip` inside (or very near) `SLAB_ALLOC`'s `GlobalAlloc::alloc`/
/// `dealloc` implementation? Used by [`ThreadHandle::interrupt`] to avoid suspending a thread
/// while it holds the global allocator's internal spinlock mid-mutation -- see that call site's
/// own doc comment for the full hazard this guards against.
///
/// This is deliberately approximate rather than exact: without loading real debug symbols at
/// runtime, there is no cheap way to get these functions' precise compiled extents. Instead,
/// this checks whether `rip` falls within a window around the entry address of EACH function
/// that actually mutates allocator state. A false positive here only costs a few extra retry
/// iterations (capped, see `MAX_ALLOCATOR_SUSPEND_RETRIES`); a false negative just means this
/// guard doesn't help for that particular call, matching today's un-guarded behavior exactly --
/// so this heuristic can only make things safer or neutral, never worse.
///
/// # Why this checks several addresses rather than one window
///
/// This function originally checked a single 512 KiB window centered on `<SafeZoneAllocator as
/// GlobalAlloc>::alloc`'s entry, on the stated assumption that one window was "wide enough to
/// comfortably cover that function, `dealloc`, and their monomorphized/inlined callees
/// (`slabmalloc`'s `ZoneAllocator::allocate`/`deallocate`, `refill`, the buddy allocator)".
///
/// That assumption was confirmed live to be FALSE, and it made this guard a no-op for almost
/// all of the time a thread genuinely spends mutating slab state. Symbolizing a real release
/// build of this runner (`llvm-symbolizer` over the shipped `.exe`) shows `alloc` at RVA
/// `0x523c00..0x524f40` and `dealloc` at `0x524f80..0x525240` -- so the old window covered
/// `0x4a3c00..0x5a3c00` -- while the linker placed every `slabmalloc::ZoneAllocator` method
/// about 4.3 MB away, entirely outside it (`deallocate` `0x9bfc00..0x9c0380`, `refill_large`
/// `0x9c03c0..0x9c06c0`, `refill` `0x9c0700..0x9c0a00`, `allocate` `0x9c0a40..0x9c27c0`). They
/// are genuinely out-of-line, not inlined as the window's author expected.
///
/// `ZoneAllocator::allocate` alone is ~7.6 KiB of code, and it is exactly where the
/// `SpinMutex`-protected page-list walks, `first_fit` bitfield scans, and partial/full/empty
/// list migrations happen. A thread suspended anywhere in that range was never detected here,
/// got its `Rip` redirected to `interrupt_callback` by the caller, and abandoned its mutation
/// partway -- corrupting the process-wide global allocator for every other thread. See
/// [`litebox::mm::allocator::slab_allocator_code_addrs`] for the addresses this now consults.
/// Whether `LITEBOX_DIAG_INTERRUPT` is set, read once per process and cached.
///
/// Deliberately not a `std::env::var_os` call at the use site. [`ThreadHandle::interrupt`] reads
/// this flag while its target thread is SUSPENDED, and on Windows `std::env::var_os` is not a
/// cheap read: it allocates (`fill_utf16_buf` grows a `Vec`, then the `OsString`) and it enters
/// ntdll's process-wide environment critical section via `RtlQueryEnvironmentVariable`. Either is
/// unrecoverable inside the suspend window -- `SuspendThread` stops the target at an arbitrary
/// instruction boundary, so it may itself be holding the environment lock or the process heap
/// lock, and a suspending thread that then blocks on one can never reach its own `ResumeThread`.
/// That is a true deadlock, not a stall: the holder cannot run until it is resumed, and the only
/// code that would resume it is the code now blocked.
///
/// Observed live, not theorised. A full MATE session under LiteBox froze with all 20 host threads
/// in `Wait`; `cdb -pv` showed exactly one thread at `Suspend: 2` (a non-invasive attach adds 1 to
/// every thread, so this one was genuinely `SuspendThread`ed) parked inside
/// `ntdll!RtlQueryEnvironmentVariable`, six more queued behind it in
/// `RtlpEnterCriticalSectionContended` on that same critical section, and the rest blocked in
/// `ThreadHandle::interrupt`. The freeze landed at a different guest pid on each run, which is
/// what a lock race looks like and what resource exhaustion does not.
fn diag_interrupt_enabled() -> bool {
    veh_gates().interrupt
}

fn rip_in_global_allocator(rip: usize) -> bool {
    // 128 KiB around each real entry point. Smaller than the old single 512 KiB window because
    // there are now several anchors covering the code that actually matters, so each one can be
    // tighter (less unrelated code false-positived) while covering strictly more allocator code
    // in total. Still comfortably larger than the largest of these functions (~7.6 KiB).
    const WINDOW: usize = 128 * 1024;
    let alloc_addr = <litebox::mm::allocator::SafeZoneAllocator<'static, 34, WindowsUserland> as core::alloc::GlobalAlloc>::alloc as *const () as usize;
    let dealloc_addr = <litebox::mm::allocator::SafeZoneAllocator<'static, 34, WindowsUserland> as core::alloc::GlobalAlloc>::dealloc as *const () as usize;
    if rip.abs_diff(alloc_addr) < WINDOW || rip.abs_diff(dealloc_addr) < WINDOW {
        return true;
    }
    if litebox::mm::allocator::slab_allocator_code_addrs()
        .iter()
        .any(|&a| rip.abs_diff(a) < WINDOW)
    {
        return true;
    }
    // See `litebox::mm::allocator::buddy_allocator_code_addrs`'s own doc comment (found live,
    // 2026-09-22, chasing the second Xvfb SIGSEGV): `buddy_system_allocator`'s `Heap::alloc`/
    // `dealloc` and the `LockedHeapWithRescue` wrappers that call them are genuinely out-of-line,
    // exactly like `ZoneAllocator`'s methods were, and mutate their OWN separate `spin::Mutex` --
    // a thread suspended inside them was just as invisible to this guard as a thread suspended
    // inside `ZoneAllocator` used to be, live-caught via a reproducible
    // `buddy_system_allocator::Heap::free_list` out-of-bounds panic under concurrent
    // cross-process-fork load.
    litebox::mm::allocator::buddy_allocator_code_addrs::<34>()
        .iter()
        .any(|&a| rip.abs_diff(a) < WINDOW)
}

impl ThreadHandle {
    /// Creates a [`ThreadHandle`] referencing the calling OS thread.
    fn for_current_thread(tls: &TlsState) -> ThreadHandle {
        let win_handle = unsafe {
            std::os::windows::io::BorrowedHandle::borrow_raw(
                windows_sys::Win32::System::Threading::GetCurrentThread(),
            )
        };
        ThreadHandle(Arc::new(Mutex::new(Some(ThreadHandleInner {
            handle: win_handle
                .try_clone_to_owned()
                .expect("failed to clone current thread handle"),
            tls: SendConstPtr(tls),
        }))))
    }

    /// Runs `f`, ensuring that [`CURRENT_THREAD_HANDLE`] is set while in the call to `f`.
    fn run_with_handle<R>(tls: &TlsState, f: impl FnOnce() -> R) -> R {
        // Safety: `tls_state` lives for the duration of this call.
        unsafe { install_tls(tls) };

        // Capture this thread's own real GS_BASE (TEB pointer) now, while it is still known-good
        // -- see `init_thread_gs_base`'s doc comment for why this repair exists at all.
        WindowsUserland::init_thread_gs_base();

        // Record this thread's own real stack reservation so a DIFFERENT thread's later
        // `Replace`-mode `allocate_pages` call (e.g. an `ET_EXEC` binary's fixed-address ELF
        // load) can detect a collision with it -- see `LIVE_THREAD_STACKS`'s doc comment.
        register_current_thread_stack();

        let handle = Self::for_current_thread(tls);
        ACTIVE_THREADS.lock().unwrap().push(handle.clone());
        CURRENT_THREAD_HANDLE.with_borrow_mut(|current| {
            assert!(
                current.is_none(),
                "thread is already registered with LiteBox",
            );
            *current = Some(handle.clone());
        });
        let _guard = litebox::utils::defer(move || {
            let current = CURRENT_THREAD_HANDLE.take().unwrap();
            // Remove from the global registry.
            ACTIVE_THREADS
                .lock()
                .unwrap()
                .retain(|h| !Arc::ptr_eq(&h.0, &current.0));
            release_all_claims_for_current_thread();
            unregister_current_thread_stack();
            *current.0.lock().unwrap() = None;
            uninstall_tls();
        });
        f()
    }

    /// Sets a pending signal on this thread, wakes it from any condvar wait,
    /// and interrupts it so the shim processes the signal promptly.
    fn deliver_signal(&self, signal: litebox_common_linux::signal::Signal) {
        let bit: u32 = 1 << (signal.as_i32() - 1);

        // Set the pending signal bit and wake the condvar in one lock scope.
        {
            let inner = self.0.lock().unwrap();
            if let Some(inner) = inner.as_ref() {
                // Safety: the TLS pointer is valid as long as the thread is
                // alive, and we hold the thread handle lock.
                let tls = unsafe { &*inner.tls.0 };
                tls.pending_host_signals.fetch_or(bit, Ordering::SeqCst);

                let waker = tls.waiting_waker.load(Ordering::Acquire);
                if !waker.is_null() {
                    // SAFETY: `waker` was heap-allocated via `Box::into_raw` in
                    // `update_waker`. It remains valid here because
                    // `update_waker` acquires this same `ThreadHandleInner`
                    // mutex before freeing the old pointer, and we hold that
                    // mutex now.
                    let waker = unsafe { &*waker };
                    waker.wake();
                }
            }
        }

        self.interrupt(None);
    }

    /// Interrupt the thread represented by this handle, where `current` is the
    /// current thread's handle if it is managed by LiteBox.
    ///
    /// The basic strategy is this:
    /// 1. Suspend the target thread.
    /// 2. Access its TLS state to check if it's in the guest.
    /// 3. If it's not actually in the guest, set the interrupt flag and resume,
    ///    with some careful handling to make sure the interrupt flag is
    ///    evaluated upon return to the guest in all cases.
    /// 4. If it is in the guest, save the guest context and set the thread
    ///    context to resume at the interrupt callback.
    /// 5. Resume the target thread.
    fn interrupt(&self, current: Option<&ThreadHandle>) {
        if diag_interrupt_enabled() {
            diag_raw_print(
                b"[diag-interrupt-enter] caller_tid=0x",
                unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() } as usize,
                b" has_current=0x",
                usize::from(current.is_some()),
            );
        }
        let (_current_guard, target) = if let Some(current) = current {
            if Arc::ptr_eq(&current.0, &self.0) {
                // Interrupting self; just set the flag.
                (unsafe { &*get_tls_ptr().unwrap() }).interrupt.set(true);
                return;
            }

            // Lock both the current and target thread handles so that this
            // thread is not suspended while holding the target thread lock.
            let (c, t) = lock_two(&current.0, &self.0);
            (Some(c), t)
        } else {
            // The current thread can't be suspended since it's not managed by LiteBox.
            (None, self.0.lock().unwrap())
        };
        let Some(inner) = target.as_ref() else {
            // The target is no longer managed by LiteBox.
            return;
        };

        // Suspend the target thread. Retry (resume, brief spin, re-suspend) if the target was
        // caught with its `Rip` inside `SafeZoneAllocator::alloc`/`dealloc` -- `SLAB_ALLOC` is a
        // single process-wide `spin::SpinMutex`-protected global allocator (see that type's own
        // doc comment), and `SuspendThread` gives no atomicity guarantee with respect to arbitrary
        // host-code instruction boundaries: catching a thread mid-mutation of shared allocator
        // metadata (between reading and writing back an internal free-list/slab pointer) and later
        // redirecting its `Rip` to `interrupt_callback` (see below) abandons that mutation
        // partway, corrupting the allocator for every other thread. Investigated live (this
        // session, a highly-reproducible `noseat`-repro capture) as the leading candidate
        // explanation for an otherwise-unexplained fault landing inside `slabmalloc::zone::
        // ZoneAllocator::allocate` with an already-corrupted return value. A thread genuinely
        // running host allocator code is never expected to run there for more than a handful of
        // instructions, so a short bounded retry loop (capped, to guarantee `interrupt` still
        // makes forward progress even if this heuristic race-loses every time) is safe and cheap.
        // Prime every lazily-initialised thing the suspend window below touches, BEFORE the
        // first `SuspendThread`. Between that suspend and its matching `ResumeThread` this thread
        // must neither allocate nor block on any lock the target could be holding -- see
        // [`diag_interrupt_enabled`] for the deadlock this prevents and how it was observed.
        // `diag_interrupt_enabled` primed itself in this function's first statement; a `OnceLock`
        // and a thread-local latch are the other two, and both take a real lock (or allocate) on
        // first touch while being a plain load afterwards.
        let _ = diag_rip0_enabled();
        let _ = is_in_ntdll_or_this(0);

        const MAX_ALLOCATOR_SUSPEND_RETRIES: u32 = 64;
        let mut attempt = 0u32;
        // Whether the retry budget ran out with the target still inside the global allocator.
        //
        // This case used to fall straight through into the context manipulation below, which is
        // exactly the thing the comment above calls out as unrecoverable: redirecting the `Rip` of
        // a thread caught mid-mutation of the process-wide `SLAB_ALLOC` abandons that mutation
        // partway and corrupts the allocator for every other thread. The budget existing at all
        // says the situation is expected; proceeding anyway when it is exhausted made the
        // expected case the fatal one.
        //
        // It is reached under real thread load. `mate-session` (dozens of threads, all
        // allocating) died reproducibly with an access violation on a loaded pointer inside
        // `fork_verify::on_single_step`, with `SafeZoneAllocator::dealloc` on the crash stack --
        // the signature of an allocator corrupted earlier by someone else. The same run under
        // `cdb` completed cleanly every time, because a debugger's serialised exception delivery
        // slows the target enough that it always leaves the allocator within the budget: a
        // Heisenbug, and the give-up path is what made it one.
        let mut still_in_allocator = false;
        loop {
            unsafe {
                windows_sys::Win32::System::Threading::SuspendThread(inner.handle.as_raw_handle());
            }
            attempt += 1;
            if attempt > MAX_ALLOCATOR_SUSPEND_RETRIES {
                still_in_allocator = true;
                break;
            }
            let mut probe_context = windows_sys::Win32::System::Diagnostics::Debug::CONTEXT {
                ContextFlags: windows_sys::Win32::System::Diagnostics::Debug::CONTEXT_CONTROL_AMD64,
                ..Default::default()
            };
            let probe_ok = unsafe {
                windows_sys::Win32::System::Diagnostics::Debug::GetThreadContext(
                    inner.handle.as_raw_handle(),
                    &raw mut probe_context,
                )
            };
            let rip = if probe_ok != 0 {
                probe_context.Rip.trunc()
            } else {
                0
            };
            if probe_ok == 0 || !rip_in_global_allocator(rip) {
                break;
            }
            // Caught mid-allocation: resume and give it a moment to finish, then retry.
            unsafe {
                windows_sys::Win32::System::Threading::ResumeThread(inner.handle.as_raw_handle());
            }
            // `yield_now` alone only helps when the target is actually runnable on this core; with
            // many runnable threads it can spin through the whole budget without the target ever
            // being scheduled. Yield for the first few attempts (cheapest, and usually enough),
            // then sleep so the target reliably gets to finish. Sleeping here is safe: the target
            // is RESUMED at this point, and this thread holds no lock it needs to make progress.
            if attempt <= 4 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(core::time::Duration::from_micros(50));
            }
        }
        if diag_interrupt_enabled() {
            diag_raw_print(
                b"[diag-interrupt-suspended] target_handle=0x",
                inner.handle.as_raw_handle() as usize,
                b" attempts=0x",
                attempt as usize,
            );
        }
        let _resume_guard = litebox::utils::defer(|| unsafe {
            windows_sys::Win32::System::Threading::ResumeThread(inner.handle.as_raw_handle());
            if diag_interrupt_enabled() {
                eprintln!("[diag-interrupt-resumed]");
            }
        });

        // SAFETY: The target TLS state is accessible while the thread is
        // suspended.
        let target_tls = unsafe { &*inner.tls.0 };

        // Write the target interrupt flag.
        //
        // Safe regardless of where the target was caught: it is a plain `Cell<bool>` store into
        // the target's own TLS, touching no shared allocator or lock.
        target_tls.interrupt.set(true);

        // The target is still inside the global allocator after the whole retry budget. Setting
        // the flag above is all that may safely be done -- redirecting its `Rip` from here would
        // abandon an in-progress allocator mutation and corrupt `SLAB_ALLOC` process-wide.
        //
        // Returning now is correct, not a fallback: the interrupt is advisory, and the flag is
        // checked before the target next returns to the guest (the `!is_in_guest` case
        // immediately below relies on exactly that property). The interrupt is therefore delivered
        // at the target's next safe point instead of immediately -- later, never wrong.
        if still_in_allocator {
            if diag_interrupt_enabled() {
                eprintln!(
                    "[diag-interrupt-deferred] target still in global allocator after {attempt} \
                     attempts; flag set, context left alone"
                );
            }
            return;
        }

        if !target_tls.is_in_guest.get() {
            // Not running in the guest. The interrupt flag will be checked
            // before returning to the guest, so just resume.
            return;
        }

        let guest_context = target_tls.guest_context_top.get().wrapping_sub(1);

        // Running in the guest. There are multiple possibilities:
        //
        // 1. The thread is in the middle of returning to the guest via the
        //    register pop path. Don't save context but do jump to the interrupt
        //    callback.
        // 2. The thread is in the middle of returning to the guest via the
        //    NtContinue path. Update the NtContinue context to point to the
        //    interrupt callback.
        // 3. The thread is beginning to handle an exception. Don't do anything;
        //    this path will check the interrupt flag.
        // 4. In the guest. Save the guest context and jump to the interrupt callback.

        // Get the current register context.
        let mut context = windows_sys::Win32::System::Diagnostics::Debug::CONTEXT {
            ContextFlags: windows_sys::Win32::System::Diagnostics::Debug::CONTEXT_CONTROL_AMD64
                | windows_sys::Win32::System::Diagnostics::Debug::CONTEXT_INTEGER_AMD64,
            ..Default::default()
        };
        let r = unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::GetThreadContext(
                inner.handle.as_raw_handle(),
                &raw mut context,
            )
        };
        assert_ne!(
            r,
            0,
            "GetThreadContext failed: {}",
            std::io::Error::last_os_error()
        );

        // 49th-pass diagnostic (buddy_system_allocator free-list corruption investigation): this
        // whole block below is reached ONLY when `target_tls.is_in_guest.get()` was true (the
        // `!is_in_guest` branch above already returned). The suspend-safety retry loop above this
        // function only protects the case where the target is caught mid-mutation of the global
        // allocator's own state; that retry loop's own probe already confirmed `rip` was NOT
        // flagged by `rip_in_global_allocator` (or the probe failed) by the time we got here. This
        // print re-checks that same predicate on the FINAL context (the one actually redirected
        // below) to catch a race between the last probe and this real `GetThreadContext`/
        // `SetThreadContext` pair, and to establish, with live evidence, whether a redirect ever
        // happens with `rip` inside/near the allocator despite `is_in_guest` being true -- the
        // open question between the fork-quiesce-routing hypothesis (a) and the data-race
        // hypothesis (b) in AGENTS.md's 48th-pass pickup.
        {
            let rip = context.Rip.trunc();
            if rip_in_global_allocator(rip) {
                // Always-on (not gated behind LITEBOX_DIAG_INTERRUPT): this specific combination
                // -- redirecting a thread's Rip while it is ALSO inside the global allocator's own
                // code -- is exactly the corruption mechanism the retry loop above exists to
                // prevent, and per its own doc comment should never reach here. If it ever does,
                // that is the smoking gun for hypothesis (a); silence here across a full boot is
                // real evidence against it. `diag_raw_print` is suspend-window-safe (no alloc/lock).
                diag_raw_print(
                    b"[diag-interrupt-GUEST-REDIRECT-IN-ALLOCATOR] rip=0x",
                    rip,
                    b" attempts=0x",
                    attempt as usize,
                );
            }
        }

        let run_interrupt_callback = if (switch_to_guest_start as *const () as usize
            ..switch_to_guest_end as *const () as usize)
            .contains(&(context.Rip.trunc()))
        {
            if diag_rip0_enabled() {
                eprintln!(
                    "[diag-interrupt] tid={:?} target_tid={:?} case=1 target_rip={:#x}",
                    std::thread::current().id(),
                    inner.handle.as_raw_handle(),
                    context.Rip,
                );
            }
            // Case 1: jump to interrupt callback without saving the guest
            // context, since it's already saved.
            true
        } else if is_in_ntdll_or_this(context.Rip.trunc()) {
            // Case 2/3: we can't distinguish between them. For case 2 we don't
            // need to do anything, but for case 3 we need to update the
            // NtContinue context to point to the interrupt callback (the guest
            // context is already up to date).
            //
            // In case 2, the NtContinue context is not being used, so it is
            // safe to update it anyway.

            // SAFETY: `continue_context` is not accessed by user-mode code
            // while `is_in_guest` is true.
            let continue_context = unsafe { &mut *target_tls.continue_context.get() };
            set_context_to_interrupt_callback(target_tls, continue_context);
            false
        } else {
            // Case 4: save the guest context and jump to interrupt callback.
            if diag_rip0_enabled() {
                eprintln!(
                    "[diag-interrupt] tid={:?} case=4 target_rip={:#x} guest_context={:#x}",
                    std::thread::current().id(),
                    context.Rip,
                    guest_context as usize,
                );
            }
            save_guest_context(unsafe { &mut *guest_context }, &context);
            true
        };
        if run_interrupt_callback {
            set_context_to_interrupt_callback(target_tls, &mut context);
            unsafe {
                windows_sys::Win32::System::Diagnostics::Debug::SetThreadContext(
                    inner.handle.as_raw_handle(),
                    &raw const context,
                );
            }
        }
    }
}

/// Updates `context` to jump to the interrupt callback with the given
/// `guest_context` pointer.
fn set_context_to_interrupt_callback(
    tls: &TlsState,
    context: &mut windows_sys::Win32::System::Diagnostics::Debug::CONTEXT,
) {
    let required_flags = windows_sys::Win32::System::Diagnostics::Debug::CONTEXT_CONTROL_AMD64
        | windows_sys::Win32::System::Diagnostics::Debug::CONTEXT_INTEGER_AMD64;
    assert_eq!(context.ContextFlags & required_flags, required_flags);
    context.Rip = interrupt_callback as *const () as usize as u64;
    context.Rsp = tls.host_sp.get().addr() as u64;
    context.Rbp = tls.host_bp.get().addr() as u64;
}

/// Returns true if the given instruction pointer is in ntdll.dll or this module.
fn is_in_ntdll_or_this(ip: usize) -> bool {
    static BOUNDS: OnceLock<[std::ops::Range<usize>; 2]> = const { OnceLock::new() };

    let bounds = BOUNDS.get_or_init(|| {
        unsafe extern "C" {
            safe static __ImageBase: c_void;
        }
        fn module_bounds(module: *const c_void) -> std::ops::Range<usize> {
            let mut module_info = windows_sys::Win32::System::ProcessStatus::MODULEINFO::default();
            let r = unsafe {
                windows_sys::Win32::System::ProcessStatus::GetModuleInformation(
                    windows_sys::Win32::System::Threading::GetCurrentProcess(),
                    module.cast_mut(),
                    &raw mut module_info,
                    size_of_val(&module_info).try_into().unwrap(),
                )
            };
            assert_ne!(
                r,
                0,
                "GetModuleInformation failed: {}",
                std::io::Error::last_os_error()
            );
            let start = module_info.lpBaseOfDll.addr();
            let end = start + module_info.SizeOfImage as usize;
            start..end
        }

        let ntdll = unsafe {
            windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(windows_sys::w!(
                "ntdll.dll"
            ))
        };
        [module_bounds(ntdll), module_bounds(&raw const __ImageBase)]
    });

    bounds.iter().any(|b| b.contains(&ip))
}

impl litebox::platform::RawMutexProvider for WindowsUserland {
    type RawMutex = RawMutex;

    fn update_waker(&self, waker: Option<litebox::event::wait::Waker<Self>>)
    where
        Self: litebox::sync::RawSyncPrimitivesProvider,
    {
        if let Some(tls) = get_tls_ptr().map(|p| unsafe { &*p }) {
            let waker_ptr = waker.map_or(std::ptr::null_mut(), |w| Box::into_raw(Box::new(w)));
            let old = tls.waiting_waker.swap(waker_ptr, Ordering::AcqRel);
            if !old.is_null() {
                // Synchronize with `deliver_signal`, which may be concurrently
                // reading the old waker pointer on another thread while holding
                // the `ThreadHandleInner` mutex. Acquiring the same mutex here
                // ensures that `deliver_signal` has finished using the pointer
                // before we free it.
                CURRENT_THREAD_HANDLE.with_borrow(|handle| {
                    let _guard = handle.as_ref().map(|handle| handle.0.lock().unwrap());
                    // SAFETY: old pointer was created by Box::into_raw in a previous
                    // call to update_waker. No other thread can be accessing it now
                    // because we synchronized via the ThreadHandleInner mutex above.
                    unsafe { drop(Box::from_raw(old)) };
                });
            }
        }
    }
}

/// Identifies one thread blocked in [`RawMutex::block_or_maybe_timeout`]: the process it lives
/// in, and the raw value of the auto-reset [`Win32_Foundation::HANDLE`] backing its per-thread
/// wait event within that process.
///
/// Stored as a plain `isize` bit pattern rather than `HANDLE` so this type stays a trivial
/// `Copy`/`Send`/`Sync` value with no `unsafe impl` needed -- the bit pattern is only ever
/// reinterpreted back into a `HANDLE` at the point of use, in [`RawMutex::resolve_waiter_event`].
///
/// # Why `pid` is here
///
/// `ADVISORY-002` §3.2 required this design to generalize to a real cross-process waiter once
/// Track B placed `RawMutex` instances in memory shared across a process boundary -- it now does
/// (`GlobalState.net` and other `litebox::sync::Mutex`-wrapped `GlobalState` fields place a
/// `RawMutex` inline in the fixed-base shared kernel arena, see `AGENTS.md` "`SharedArc<T>` and
/// real `GlobalState` create-vs-attach"): a waiter's raw event handle is only a meaningful value
/// inside the process that created it, so a waker resolving one owned by a *different* process
/// must go through [`RawMutex::resolve_waiter_event`]'s `DuplicateHandle` path rather than ever
/// calling `SetEvent` directly on a foreign handle number. `pid` here is always the real
/// registering process's own id, live-checked at registration time and never touched again --
/// see [`WaiterQueue`]'s doc comment for the storage-layout bug that used to corrupt it in the
/// reading (waking) process, now fixed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct WaiterRecord {
    pid: u32,
    event: isize,
}

/// Sentinel `pid` marking a [`WaiterSlot`] as unoccupied. Real Windows process ids are never `0`
/// (reserved for the System Idle Process), so this cannot collide with a genuine waiter.
const WAITER_SLOT_EMPTY: u32 = 0;

/// How many threads may block on one [`RawMutex`] at the same time. See [`WaiterQueue`]'s doc
/// comment for why this is a fixed inline array rather than a `Vec`; 32 concurrent blocked waiters
/// on a single specific mutex is already an extreme contention scenario for one guest.
const MAX_INLINE_WAITERS: usize = 32;

/// How long [`RawMutex::block_or_maybe_timeout`] lets a wait run with no wake before checking
/// whether the recorded holder ([`RawMutex::holder_pid`]) is still alive. Deliberately generous --
/// this is a "genuinely stuck" threshold, not a normal contention latency budget: a live holder,
/// however slow, is never disturbed by this, only queried, so the only cost of a larger value here
/// is how long a genuinely orphaned lock (the holder process already dead) stays stuck before a
/// waiter notices; the only cost of a smaller one is a few more harmless `OpenProcess`/
/// `GetExitCodeProcess` calls per still-live long wait. See [`RawMutex::holder_pid`]'s doc comment
/// for the live orphaned-lock defect this interval exists to bound.
const LIVENESS_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// Windows' `STILL_ACTIVE` sentinel (`GetExitCodeProcess` returns this as the "exit code" for a
/// process that has not yet terminated) -- duplicated from `process_fork.rs`'s own identically-
/// named, identically-valued local constant rather than shared, matching that constant's own doc
/// comment on why: this crate's `windows_sys` version does not export it.
const STILL_ACTIVE: u32 = 259;

struct WaiterSlot {
    pid: AtomicU32,
    event: core::sync::atomic::AtomicIsize,
}

impl WaiterSlot {
    const fn empty() -> Self {
        Self {
            pid: AtomicU32::new(WAITER_SLOT_EMPTY),
            event: core::sync::atomic::AtomicIsize::new(0),
        }
    }
}

/// Fixed-capacity, pointer-free wait queue backing one [`RawMutex`]'s blocked waiters.
///
/// # Why not `Mutex<Vec<WaiterRecord>>` (the earlier design)
///
/// That design is correct only as long as every `RawMutex` instance stays in this process's own
/// private heap. It no longer does: `RawMutex` can be embedded directly inside cross-process-shared
/// memory today (e.g. `GlobalState.net: litebox::sync::Mutex<Platform, Network<Platform>>`, whose
/// bytes -- the inline `RawMutex` included -- sit in the fixed-base shared kernel arena via
/// `SharedArc`, see `AGENTS.md` "`SharedArc<T>` and real `GlobalState` create-vs-attach"). A `Vec`'s
/// backing buffer is allocated on the ordinary process-private heap by whichever process calls
/// `push`, so a different, attaching process reading that same shared struct dereferences a pointer
/// meaningful only in the FIRST process's address space -- the ninth confirmed instance of this
/// project's nested-collection-on-private-heap defect class (`elf_patch_cache`/`exec_ranges_cache`/
/// `segment_scan_cache`/the `GlobalStateHandle`-shadowed registries/`litebox` field, all documented
/// in `AGENTS.md`). Confirmed live: a `cdb -p`-attached thread genuinely blocked in
/// [`RawMutex::block`] via fork-synchronization code, and the WAKING thread's `wake_many` -> read
/// of that same shared `Vec` decoded a bogus small `pid` (`8`, not any real litebox process) rather
/// than the real registering process's id -- `resolve_waiter_event` then panicked on
/// `OpenProcess(pid=8)` failing, and because that panic happened on the WAKER's side, the genuine
/// waiter was never signaled and hung forever (a real lost-wakeup, not a timing artifact).
///
/// # The fix
///
/// Every waiter's bytes live inline in fixed slots that are part of `RawMutex`'s own byte range, so
/// they travel with it regardless of which process's heap that range happens to be embedded in --
/// the same "flat, pointer-free redesign" pattern already used for `SharedUnixAddrPresenceTable`.
/// `lock` is a pure atomic spin-CAS, not `std::sync::Mutex`: an OS-backed lock embedded in the same
/// shared bytes would reintroduce the exact process-local-wakeup defect (MSDN: the
/// `WaitOnAddress`/SRWLOCK-family primitives only wake threads in the SAME process) this whole
/// `RawMutex` rewrite exists to fix, one layer down. `lock` is held only across a handful of atomic
/// slot reads/writes, never across a syscall or a wait, so unbounded spinning is safe and bounded in
/// practice.
struct WaiterQueue {
    lock: core::sync::atomic::AtomicBool,
    slots: [WaiterSlot; MAX_INLINE_WAITERS],
}

impl WaiterQueue {
    const fn new() -> Self {
        const EMPTY: WaiterSlot = WaiterSlot::empty();
        Self {
            lock: core::sync::atomic::AtomicBool::new(false),
            slots: [EMPTY; MAX_INLINE_WAITERS],
        }
    }

    /// Runs `f` with this queue's spinlock held. The whole point of this type: callers use this to
    /// make "check `inner`'s value, then register/pop a waiter" one atomic critical section, exactly
    /// as the earlier `Mutex<Vec<_>>` design did -- see `block_or_maybe_timeout`'s and `wake_many`'s
    /// own doc comments for the lost-wakeup argument this preserves.
    fn with_lock<R>(&self, f: impl FnOnce(&Self) -> R) -> R {
        while self
            .lock
            .compare_exchange_weak(
                false,
                true,
                core::sync::atomic::Ordering::Acquire,
                core::sync::atomic::Ordering::Relaxed,
            )
            .is_err()
        {
            core::hint::spin_loop();
        }
        let result = f(self);
        self.lock
            .store(false, core::sync::atomic::Ordering::Release);
        result
    }

    /// Registers `record` in the first free slot. Returns `false` (never panics -- this is
    /// guest-reachable machinery, and a panic here would kill every guest process at once, per
    /// `AGENTS.md`'s standing "guest-reachable code returns an errno, never a panic" rule) if every
    /// slot is occupied; the caller falls back to polling.
    #[must_use]
    fn push_locked(&self, record: WaiterRecord) -> bool {
        for slot in &self.slots {
            if slot
                .pid
                .load(core::sync::atomic::Ordering::Relaxed)
                == WAITER_SLOT_EMPTY
            {
                slot.event
                    .store(record.event, core::sync::atomic::Ordering::Relaxed);
                slot.pid
                    .store(record.pid, core::sync::atomic::Ordering::Relaxed);
                return true;
            }
        }
        false
    }

    /// Finds and removes the exact `record`, reporting whether it was still queued. Used by the
    /// timeout-race path in `block_or_maybe_timeout`.
    fn remove_locked(&self, record: WaiterRecord) -> bool {
        if record.pid == WAITER_SLOT_EMPTY {
            return false;
        }
        for slot in &self.slots {
            if slot.pid.load(core::sync::atomic::Ordering::Relaxed) == record.pid
                && slot.event.load(core::sync::atomic::Ordering::Relaxed) == record.event
            {
                slot.pid
                    .store(WAITER_SLOT_EMPTY, core::sync::atomic::Ordering::Relaxed);
                return true;
            }
        }
        false
    }

    /// Pops up to `n` occupied slots. The returned `Vec` is a purely local, transient result built
    /// and consumed within this one call on this one process -- never written back into `self` (a
    /// possibly cross-process-shared struct), so it carries none of the staleness risk the old
    /// `Vec<WaiterRecord>` field itself had.
    fn drain_locked(&self, n: usize) -> Vec<WaiterRecord> {
        let mut popped = Vec::new();
        for slot in &self.slots {
            if popped.len() >= n {
                break;
            }
            let pid = slot.pid.load(core::sync::atomic::Ordering::Relaxed);
            if pid != WAITER_SLOT_EMPTY {
                let event = slot.event.load(core::sync::atomic::Ordering::Relaxed);
                slot.pid
                    .store(WAITER_SLOT_EMPTY, core::sync::atomic::Ordering::Relaxed);
                popped.push(WaiterRecord { pid, event });
            }
        }
        popped
    }
}

struct ThreadWaiterEvent(Win32_Foundation::HANDLE);

impl Drop for ThreadWaiterEvent {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid, owned event handle, not used after this point. No other
        // thread can be blocked on it: it is this thread's own per-thread event, and a thread
        // cannot simultaneously be running this destructor (on itself, at thread-exit) and
        // blocked inside `RawMutex::block_or_maybe_timeout` (also on itself).
        unsafe {
            Win32_Foundation::CloseHandle(self.0);
        }
    }
}

thread_local! {
    /// This OS thread's own auto-reset wait event, created lazily on first use and reused for
    /// every [`RawMutex`] this thread ever blocks on, for its whole lifetime -- replacing
    /// `WaitOnAddress`/`WakeByAddressSingle` (MSDN: wakeable only by "another thread in the same
    /// process"; `ADVISORY-002` §3.2) with the one primitive that generalizes across a process
    /// boundary: a kernel `Event` object. One event per THREAD rather than one per `RawMutex`
    /// keeps this independent of how many locks a thread ever contends on, and keeps `RawMutex`
    /// itself `const`-constructible ([`litebox::platform::RawMutex::INIT`]), since opening a
    /// kernel object is not a `const` operation.
    static THREAD_WAITER_EVENT: RefCell<Option<ThreadWaiterEvent>> = const { RefCell::new(None) };
}

/// Returns this thread's own cached waiter event, creating it on first call.
///
/// The event is deliberately unnamed: it is signaled either directly by another thread in this
/// same process (the common case) or via a `DuplicateHandle`'d copy held by a thread in another
/// process ([`RawMutex::resolve_waiter_event`], real and live now for `RawMutex` instances
/// embedded in cross-process-shared memory) -- neither needs the
/// object to be findable by name.
fn thread_waiter_event() -> Win32_Foundation::HANDLE {
    THREAD_WAITER_EVENT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(existing) = slot.as_ref() {
            return existing.0;
        }
        // SAFETY: a null name requests an unnamed event; bManualReset=FALSE (auto-reset) and
        // bInitialState=FALSE (not signaled) are required -- see this function's doc comment and
        // `RawMutex::block_or_maybe_timeout`'s lost-wakeup argument, both of which depend on
        // auto-reset semantics.
        let handle =
            unsafe { Win32_Threading::CreateEventW(core::ptr::null(), 0, 0, core::ptr::null()) };
        assert!(
            !handle.is_null(),
            "CreateEventW for thread waiter event failed: {}",
            unsafe { GetLastError() }
        );
        *slot = Some(ThreadWaiterEvent(handle));
        handle
    })
}

/// A raw mutex/futex for Windows.
///
/// Blocking is implemented with a manual wait queue plus a per-waiter kernel `Event`
/// ([`thread_waiter_event`]) rather than `WaitOnAddress`/`WakeByAddressSingle`, because the
/// latter pair is process-local by construction (`ADVISORY-002` §3.2) and this primitive must be
/// ready to cross a process boundary once Track B's fixed-base shared kernel heap (step 3)
/// lands. `inner` -- the value exposed as [`litebox::platform::RawMutex::underlying_atomic`] --
/// is unchanged: it remains the uncontended fast-path CAS target for every caller, exactly as
/// before this change.
pub struct RawMutex {
    // The `inner` is the value shown to the outside world as an underlying atomic.
    inner: AtomicU32,
    /// Threads currently blocked in [`Self::block_or_maybe_timeout`] on this specific `RawMutex`.
    /// A fixed-capacity, pointer-free [`WaiterQueue`], NOT `Mutex<Vec<WaiterRecord>>` -- `RawMutex`
    /// instances can be embedded directly in cross-process-shared memory today (Track B's shared
    /// kernel arena, e.g. `GlobalState.net`); see [`WaiterQueue`]'s doc comment for the real,
    /// live-confirmed bug that design change fixes.
    waiters: WaiterQueue,
    /// The Windows process id of whichever thread most recently ran
    /// [`litebox::platform::RawMutex::note_locked`] on this instance, or `0` if unlocked/unknown.
    /// Written only by the current holder (`note_locked`/`note_unlocked`), read only by a blocked
    /// waiter in [`Self::block_or_maybe_timeout`]'s periodic liveness check -- see that method's
    /// doc comment for the orphaned-lock defect this exists to recover from. Deliberately just a
    /// pid, not a `WaiterRecord`-style `(pid, event)` pair: the holder is not necessarily blocked
    /// anywhere, so it has no waiter event to report, and none is needed -- a waiter that decides
    /// to recover only needs to know whether this pid is still alive, never to signal it directly.
    holder_pid: AtomicU32,
    /// Set by [`Self::try_recover_from_dead_holder_unregistered`] when it forces this lock back
    /// open because its recorded holder was confirmed dead. Forcing the `inner` word back to
    /// unlocked is necessary so no thread waits forever, but it does nothing to repair whatever
    /// the dead holder's own critical section was mid-way through mutating -- that data can be
    /// left torn (partially-applied) with no general way for `RawMutex` itself (generic over
    /// every `litebox::sync::Mutex<Platform, T>` in the codebase, with no idea what `T` is) to
    /// repair it. This flag is the hand-off: [`Self::take_poison`] lets the specific caller that
    /// DOES know how to reset its own `T` to a safe default (currently only `Network`'s
    /// `GlobalStateHandle::net_lock`, see its own doc comment) find out it must do so, exactly
    /// once per recovery event.
    poisoned: core::sync::atomic::AtomicBool,
}

/// Process-local cache of cross-process handle duplications [`RawMutex::resolve_waiter_event`] has
/// ever needed, keyed by the resolving `RawMutex`'s own address plus the [`WaiterRecord`] resolved
/// for it. Deliberately NOT a field of [`RawMutex`] itself (an earlier design had it as one,
/// `remote_waiter_handles: Mutex<Vec<(WaiterRecord, isize)>>`): `RawMutex` can be embedded directly
/// in cross-process-shared memory, and a `DuplicateHandle`d local `HANDLE` value is only meaningful
/// in the process that created it -- storing it inline would let a second process read a handle
/// value the FIRST process fabricated for itself, and potentially `SetEvent` an unrelated handle
/// that happens to share that same numeric value in the second process's own handle table. Keying
/// by `self`'s address is safe even though the same `RawMutex` may live at the same address in every
/// process that has it mapped (Track B's shared kernel arena is fixed-base): this cache is itself a
/// process-local `static`, so no two processes ever share one instance of it.
static REMOTE_WAITER_HANDLES: OnceLock<Mutex<Vec<(usize, WaiterRecord, isize)>>> = OnceLock::new();

impl RawMutex {
    const fn new() -> Self {
        Self {
            inner: AtomicU32::new(0),
            waiters: WaiterQueue::new(),
            holder_pid: AtomicU32::new(0),
            poisoned: core::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Returns a `HANDLE` this process can legally call `SetEvent` on for `record`, or `None` if
    /// `record`'s owning process is no longer reachable (see below).
    ///
    /// # The two cases
    ///
    /// - `record.pid` is this process's own id: the raw `record.event` value is already directly
    ///   usable, because a `HANDLE` is valid for any thread within its owning process, not just the
    ///   thread that created it.
    /// - `record.pid` names a *different* process (real and live as of `WaiterQueue`'s fix --
    ///   `RawMutex` can now be embedded in cross-process-shared memory, see that type's doc
    ///   comment): `record.event`'s bit pattern is meaningless in this process's own handle table,
    ///   so it must be imported via `OpenProcess` + `DuplicateHandle` -- the same mechanism
    ///   `advisor/probes/dup_probe.c` and `control_server.rs` already prove works for a same-user,
    ///   non-admin sibling process. The result is cached in the process-local
    ///   [`REMOTE_WAITER_HANDLES`] so a mutex with a steady set of cross-process waiters pays the
    ///   duplication cost once, not on every wake.
    ///
    /// # Returning `None` instead of panicking
    ///
    /// `record.pid` was read from a `WaiterRecord` this same `RawMutex`'s own
    /// `block_or_maybe_timeout` pushed while that waiter was live, so under `WaiterQueue`'s
    /// pointer-free fix it is always the real registering process's id -- but that process can
    /// legitimately have exited between registering and this wake (a fatal host fault, an
    /// `execve`-replaced process, ordinary process teardown racing a wake). `OpenProcess`/
    /// `DuplicateHandle` failing in that case means "the waiter is already gone, nothing to wake",
    /// not a bug -- and per `AGENTS.md`'s standing "guest-reachable code returns an errno, never a
    /// panic" rule (this is on the wake path of every contended lock in the shim), the caller must
    /// be able to skip a single unresolvable waiter rather than aborting the whole wake and, as the
    /// earlier `assert!`-based version did, leaving every OTHER already-popped waiter unsignaled
    /// forever too.
    fn resolve_waiter_event(&self, record: WaiterRecord) -> Option<Win32_Foundation::HANDLE> {
        // SAFETY: reading the calling thread's own process id; no preconditions.
        let self_pid = unsafe { Win32_Threading::GetCurrentProcessId() };
        if record.pid == self_pid {
            return Some(record.event as Win32_Foundation::HANDLE);
        }

        let self_addr = core::ptr::from_ref(self) as usize;
        let cache = REMOTE_WAITER_HANDLES.get_or_init(|| Mutex::new(Vec::new()));
        {
            let cache = cache.lock().unwrap();
            if let Some((_, _, local)) = cache
                .iter()
                .find(|(addr, r, _)| *addr == self_addr && *r == record)
            {
                return Some(*local as Win32_Foundation::HANDLE);
            }
        }

        // SAFETY: `record.pid` was read from a `WaiterRecord` this same mutex's own
        // `block_or_maybe_timeout` pushed while that waiter was live; `PROCESS_DUP_HANDLE` is the
        // minimum access the `DuplicateHandle` call below needs.
        let owner = unsafe {
            Win32_Threading::OpenProcess(Win32_Threading::PROCESS_DUP_HANDLE, 0, record.pid)
        };
        if owner.is_null() {
            let last_error = unsafe { GetLastError() };
            litebox_util_log::warn!(
                pid:% = record.pid,
                win32_error:% = last_error;
                "resolve_waiter_event: OpenProcess(PROCESS_DUP_HANDLE) failed -- waiter's process is gone, skipping its wake"
            );
            return None;
        }

        let mut local: Win32_Foundation::HANDLE = core::ptr::null_mut();
        // SAFETY: `owner` was just opened with `PROCESS_DUP_HANDLE` above; `record.event` is a
        // live event handle in that process (its owning thread cannot have closed it while
        // registered as a waiter, for the same reason `wake_many`'s `SetEvent` call is safe --
        // see that method's doc comment) unless that process has already exited, which
        // `DuplicateHandle` below reports as failure rather than UB; `local` is a valid out-pointer
        // into this process's own handle table.
        let ok = unsafe {
            Win32_Foundation::DuplicateHandle(
                owner,
                record.event as Win32_Foundation::HANDLE,
                GetCurrentProcess(),
                &mut local,
                0,
                0,
                Win32_Foundation::DUPLICATE_SAME_ACCESS,
            )
        };
        // SAFETY: `owner` is a valid, owned handle not used again after this point.
        unsafe {
            Win32_Foundation::CloseHandle(owner);
        }
        if ok == 0 {
            let last_error = unsafe { GetLastError() };
            litebox_util_log::warn!(
                pid:% = record.pid,
                win32_error:% = last_error;
                "resolve_waiter_event: DuplicateHandle(waiter event) failed -- waiter's process is gone, skipping its wake"
            );
            return None;
        }

        cache.lock().unwrap().push((self_addr, record, local as isize));
        Some(local)
    }

    #[expect(clippy::unnecessary_wraps)]
    fn block_or_maybe_timeout(
        &self,
        val: u32,
        timeout: Option<Duration>,
    ) -> Result<UnblockedOrTimedOut, ImmediatelyWokenUp> {
        // `LITEBOX_DIAG_WAIT_DUR=1`: log requested-vs-actual duration for every wait whose
        // actual elapsed time is itself suspiciously long (>=1s), regardless of whether it timed
        // out or was woken. This is the decisive measurement for the "missed wakeup, rescued
        // only by a timeout" theory (AGENTS.md, "Rendering/scanout blocker": stalls recur in
        // precise 60.000s-period blocks) -- a genuine missed-wakeup shows requested==actual
        // (always times out, never woken early), whereas a real wake arriving late but before
        // the timeout shows actual<requested. Gated and gap-filtered to avoid flooding on the
        // (extremely common) fast/normal wait case.
        let diag = diag_wait_dur_enabled();
        let start = diag.then(std::time::Instant::now);

        let event = thread_waiter_event();
        // SAFETY: reading the calling thread's own process id; no preconditions.
        let pid = unsafe { Win32_Threading::GetCurrentProcessId() };
        let record = WaiterRecord {
            pid,
            event: event as isize,
        };

        // Register-then-check under the SAME lock `wake_many` takes to check-then-pop: this is
        // the lost-wakeup argument. Whichever of (this check) / (a concurrent release's value
        // change followed by its `wake_many` call) happens second, while holding this lock,
        // observes the other's effect -- there is no window in which this thread has "decided to
        // wait" without yet being visible to a waker, because registering and checking are one
        // critical section.
        enum Registration {
            AlreadyChanged,
            Registered,
            QueueFull,
        }
        let registration = self.waiters.with_lock(|queue| {
            if self.inner.load(Ordering::SeqCst) != val {
                Registration::AlreadyChanged
            } else if queue.push_locked(record) {
                Registration::Registered
            } else {
                Registration::QueueFull
            }
        });
        match registration {
            Registration::AlreadyChanged => return Ok(UnblockedOrTimedOut::Unblocked),
            Registration::Registered => {}
            Registration::QueueFull => {
                // Every one of `MAX_INLINE_WAITERS` slots on this specific `RawMutex` is occupied
                // -- extremely rare in practice (see `WaiterQueue`'s doc comment). This thread was
                // never registered, so it cannot rely on `wake_many` ever signaling its event;
                // falling back to polling `inner` directly is the only option that neither panics
                // (guest-reachable) nor risks a permanent lost wakeup.
                litebox_util_log::warn!(
                    max_waiters:% = MAX_INLINE_WAITERS;
                    "RawMutex::block_or_maybe_timeout: waiter queue full, falling back to polling"
                );
                return Ok(self.poll_until_value_changes(val, timeout));
            }
        }

        // Wait in bounded chunks -- never longer than `LIVENESS_CHECK_INTERVAL` -- even for a
        // caller-requested infinite/no-timeout wait (`timeout == None`, i.e. every call reached
        // via `litebox::platform::RawMutex::block`, which `litebox::sync::mutex::
        // SpinEnabledRawMutex::lock_contended` uses for EVERY genuinely contended lock acquisition
        // in this whole codebase). This is what lets `try_recover_from_dead_holder` ever run at
        // all for a `block()` caller; see `holder_pid`'s doc comment for the live orphaned-lock
        // defect this recovers from. It changes nothing about how promptly a genuine wake is
        // observed: `WaitForSingleObject` still returns the instant a concurrent `wake_many` calls
        // `SetEvent`, regardless of chunk size -- chunking only matters once a whole
        // `LIVENESS_CHECK_INTERVAL` has passed with no wake at all.
        let overall_deadline = timeout.map(|t| std::time::Instant::now() + t);
        let result = loop {
            let remaining = overall_deadline
                .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()));
            if remaining == Some(Duration::ZERO) {
                break self.finish_real_timeout(record, event);
            }
            let chunk = remaining.map_or(LIVENESS_CHECK_INTERVAL, |r| r.min(LIVENESS_CHECK_INTERVAL));
            let chunk_ms = chunk
                .as_millis()
                .min(u128::from(Win32_Threading::INFINITE - 1))
                .trunc();

            // SAFETY: `event` is this thread's own valid, owned-for-its-lifetime event handle.
            let rc = unsafe { Win32_Threading::WaitForSingleObject(event, chunk_ms) };

            match rc {
                Win32_Foundation::WAIT_OBJECT_0 => break Ok(UnblockedOrTimedOut::Unblocked),
                Win32_Foundation::WAIT_TIMEOUT => {
                    let real_deadline_passed = overall_deadline
                        .is_some_and(|deadline| deadline <= std::time::Instant::now());
                    if real_deadline_passed {
                        break self.finish_real_timeout(record, event);
                    }
                    // Not a real (caller-requested) timeout yet -- just one more
                    // `LIVENESS_CHECK_INTERVAL` chunk elapsing with no wake. Recover only if the
                    // recorded holder is POSITIVELY CONFIRMED dead; otherwise keep waiting exactly
                    // as an infinite wait always did before this change.
                    if self.try_recover_from_dead_holder(val, record) {
                        break Ok(UnblockedOrTimedOut::Unblocked);
                    }
                }
                Win32_Foundation::WAIT_FAILED => {
                    let err = unsafe { GetLastError() };
                    panic!("Unexpected error={err} for WaitForSingleObject")
                }
                other => panic!("Unexpected WaitForSingleObject return {other:#x}"),
            }
        };

        if let Some(start) = start {
            let elapsed = start.elapsed();
            if elapsed >= Duration::from_secs(1) {
                // `ThreadId` and `UnblockedOrTimedOut` are Debug-only, not Display,
                // so both must use `:?` here.
                litebox_util_log::error!(
                    tid:? = std::thread::current().id(),
                    requested:? = timeout,
                    elapsed_ms:% = elapsed.as_millis(),
                    result:? = result;
                    "[diag-wait-dur] wait_for_single_object"
                );
            }
        }

        result
    }

    /// Handles a REAL (caller-requested, via `block_or_timeout`) timeout expiring in
    /// `block_or_maybe_timeout`'s wait loop -- factored out so the loop's periodic internal
    /// `LIVENESS_CHECK_INTERVAL` chunk timeouts (which are not real timeouts at all, see that
    /// loop's own comment) share this exact, pre-existing race-resolution logic with the real one,
    /// unchanged from before this method was split out.
    fn finish_real_timeout(
        &self,
        record: WaiterRecord,
        event: Win32_Foundation::HANDLE,
    ) -> Result<UnblockedOrTimedOut, ImmediatelyWokenUp> {
        // Race with a concurrent `wake_many`: it may have already popped `record` (and
        // therefore committed to signaling `event`) in the gap between the wait timing
        // out internally and this thread reacquiring `self.waiters`. Resolve it under
        // the same lock, so the two operations are mutually exclusive.
        let still_queued = self.waiters.with_lock(|queue| queue.remove_locked(record));
        if still_queued {
            Ok(UnblockedOrTimedOut::TimedOut)
        } else {
            // `wake_many` already removed us from the queue, which under its own
            // implementation happens only as part of unconditionally signaling `event`
            // (see its doc comment) -- so this wait cannot race that signal, only
            // possibly precede its delivery. Consuming it here, rather than leaving it
            // pending on a per-thread event this thread will reuse for an unrelated
            // future wait, is what makes that reuse across calls safe.
            // SAFETY: `event` is this thread's own valid, owned event handle.
            let rc2 =
                unsafe { Win32_Threading::WaitForSingleObject(event, Win32_Threading::INFINITE) };
            assert_eq!(
                rc2,
                Win32_Foundation::WAIT_OBJECT_0,
                "waiter event not signaled after wake_many committed to signaling it"
            );
            Ok(UnblockedOrTimedOut::Unblocked)
        }
    }

    /// Checked every [`LIVENESS_CHECK_INTERVAL`] by a thread that has been blocked in
    /// [`Self::block_or_maybe_timeout`] without a wake for that whole interval: is the process
    /// recorded in [`Self::holder_pid`] (the most recent [`litebox::platform::RawMutex::
    /// note_locked`] caller) still alive? If it is confirmably dead (`OpenProcess` fails, or
    /// `GetExitCodeProcess` reports anything other than `STILL_ACTIVE`), this `RawMutex` is
    /// orphaned -- exactly the live, `cdb`-confirmed defect [`Self::holder_pid`]'s own doc comment
    /// describes: a cross-process-fork child's fast `ExitProcess` exit path is documented
    /// (`litebox_runner_linux_on_windows_userland::main`) to skip ordinary `Drop`-based unlocking,
    /// so a child killed while its own `net_worker` thread held a genuinely cross-process-shared
    /// `litebox::sync::Mutex` (`GlobalStateHandle::net_lock`'s, in the reproduced 10-sequential-
    /// `mkdir`-cross-process-fork repro) leaves that mutex's `inner` word permanently at `val` (1
    /// or 2) with no live thread anywhere ever going to call `unlock()` on it again -- confirmed
    /// live via `cdb -p`, two snapshots ~18s apart showing the identical stuck stack (`run::
    /// {closure#0}` -> `Mutex::lock_contended` -> `RawMutex::block` -> `WaitForSingleObjectEx`),
    /// with every other thread in the process idle-waiting or independently progressing, never the
    /// lock holder.
    ///
    /// Recovery here is deliberately conservative: `holder_pid == 0` (never recorded, or already
    /// cleared by a normal `unlock()`) is treated as "unknown, do not guess" and never recovered --
    /// only a POSITIVELY CONFIRMED-DEAD recorded holder is ever forced open. A holder confirmed
    /// still alive is left completely alone: this never steals a lock out from under a live,
    /// legitimately slow holder (heavy real contention, or a slow first-time `NatGateway` init) --
    /// doing so would be a correctness disaster (two threads/processes believing they hold the same
    /// critical section at once), not a fix. This is also why recovery is gated behind a multi-
    /// second `LIVENESS_CHECK_INTERVAL` rather than firing immediately on first contention: the
    /// common case (a live, busy holder) must never pay for this at all, only a wait that has
    /// already gone on unusually long does.
    ///
    /// Returns `true` if it recovered the lock (the caller should stop waiting on `event` and let
    /// the normal CAS retry loop in `litebox::sync::mutex::SpinEnabledRawMutex::lock_contended`
    /// re-attempt acquisition, exactly as it would after any other spurious wake), `false` if the
    /// holder is confirmed alive or unknown (the caller should keep waiting).
    fn try_recover_from_dead_holder(&self, val: u32, record: WaiterRecord) -> bool {
        if !self.try_recover_from_dead_holder_unregistered(val) {
            return false;
        }
        // Remove this thread's own registration: it is no longer going to wait on `event` for this
        // call, so a later, unrelated `wake_many` must not find and signal a stale record for it.
        // Best-effort (a concurrent `wake_many` may already have popped it, which is fine -- see
        // `finish_real_timeout`'s identical race handling for the real-timeout case).
        self.waiters.with_lock(|queue| queue.remove_locked(record));
        true
    }

    /// Core of [`Self::try_recover_from_dead_holder`] (see that function's own doc comment for the
    /// full defect this recovers from, and why recovery is conservative -- only a
    /// positively-confirmed-dead recorded holder is ever forced open), factored out so
    /// [`Self::poll_until_value_changes`] -- the `QueueFull` fallback for a caller that could not
    /// register in [`Self::waiters`] at all, and therefore has no [`WaiterRecord`] to later remove
    /// -- can also run it.
    ///
    /// Before this existed, `poll_until_value_changes` had NO liveness check at all: unlike every
    /// registered waiter on the same `RawMutex` (which gets this same dead-holder check every
    /// [`LIVENESS_CHECK_INTERVAL`] via [`Self::try_recover_from_dead_holder`]), a thread that
    /// overflowed the 32-slot [`WaiterQueue`] and fell back to this function would poll
    /// `self.inner` forever with no way to ever detect or recover an orphaned lock -- silently
    /// reintroducing, for exactly this one fallback path, the same permanent-orphan defect
    /// `try_recover_from_dead_holder` itself was written to fix for every other waiter. Live,
    /// `cdb`-confirmed 2026-09-17 (Track B, post-`Network::socket_set` shared-arena fix pass):
    /// making socket state genuinely cross-process-shared created enough real contention on a
    /// single `RawMutex` (`webtop_stack.sh`'s nginx self-test) to overflow this queue for the
    /// first time in a real boot (`RawMutex::block_or_maybe_timeout: waiter queue full, falling
    /// back to polling`, 4 occurrences, zero on the pre-fix binary in an identical A/B boot), with
    /// the boot then stalling permanently past `NGINX_STARTED`.
    fn try_recover_from_dead_holder_unregistered(&self, val: u32) -> bool {
        let holder = self.holder_pid.load(Ordering::Acquire);
        if holder == 0 {
            return false;
        }
        // SAFETY: liveness probe only, minimum access requested.
        let handle = unsafe {
            Win32_Threading::OpenProcess(
                Win32_Threading::PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                holder,
            )
        };
        let dead = if handle.is_null() {
            true
        } else {
            let mut exit_code: u32 = 0;
            // SAFETY: `handle` was just successfully opened above.
            let ok = unsafe { Win32_Threading::GetExitCodeProcess(handle, &raw mut exit_code) };
            // SAFETY: `handle` is a valid, owned handle not used again after this point.
            unsafe {
                Win32_Foundation::CloseHandle(handle);
            }
            ok != 0 && exit_code != STILL_ACTIVE
        };
        if !dead {
            return false;
        }
        litebox_util_log::warn!(
            holder_pid:% = holder, val:% = val;
            "RawMutex::poll_until_value_changes: recorded holder process is dead -- recovering orphaned lock (queue-full fallback path)"
        );
        // Best-effort: force the stuck value back to unlocked so the normal CAS retry loop can
        // proceed. A `compare_exchange` (not a plain `store`) so a concurrent recovery by another
        // waiter, or the (extremely unlikely, but not impossible) case that the real holder's
        // process id was reused by an unrelated new process between the read above and here,
        // cannot clobber a value someone else has already legitimately changed.
        let _ = self
            .inner
            .compare_exchange(val, 0, Ordering::AcqRel, Ordering::Relaxed);
        // Only clear `holder_pid` if it is still the same dead pid just confirmed -- never clobber
        // a DIFFERENT holder that may have legitimately acquired the lock in the interim.
        let _ =
            self.holder_pid
                .compare_exchange(holder, 0, Ordering::AcqRel, Ordering::Relaxed);
        // Unconditional `store`, not a CAS: unlike `inner`/`holder_pid` above (which must not
        // clobber a legitimate new holder/value), poisoning is monotonic within one recovery
        // event -- once this recovery happened, the protected data really may be torn, and that
        // fact must survive even if another thread's concurrent recovery attempt (extremely
        // unlikely, but see the comments above) already cleared/reset `holder_pid` first. Losing a
        // poison signal would let a caller trust torn state; a spurious extra one only costs a
        // single unnecessary safe reset.
        self.poisoned.store(true, Ordering::Release);
        true
    }

    /// Atomically reads and clears the poison flag [`Self::try_recover_from_dead_holder_unregistered`]
    /// sets on a dead-holder recovery -- see [`Self::poisoned`]'s own doc comment for the defect
    /// this exists to hand off. `swap` (not `load` then `store`) so the read-and-clear is one
    /// atomic step: at most one caller ever observes `true` for a given poisoning event, matching
    /// the exclusivity the mutex itself already guarantees inside an ordinary critical section --
    /// whichever thread's `lock()` call is the first to win the CAS race after recovery is the one
    /// that must reset the protected data, and every OTHER concurrent locker must NOT also reset
    /// it out from under that thread's fresh, already-safe state.
    fn take_poison(&self) -> bool {
        self.poisoned.swap(false, Ordering::AcqRel)
    }

    /// Fallback for the (extremely rare, see `WaiterQueue`'s doc comment) case where
    /// `block_or_maybe_timeout` could not register in the waiter queue at all: polls `inner`
    /// directly rather than relying on any wake delivery. Correct by construction (immune to any
    /// bug in the wake path, at the cost of latency/CPU while polling), never loses a wakeup.
    ///
    /// Also runs [`Self::try_recover_from_dead_holder_unregistered`] every
    /// [`LIVENESS_CHECK_INTERVAL`] -- see that function's own doc comment for why this loop must
    /// not be a bare `inner`-changed check: without it, this path has no way to ever detect or
    /// recover an orphaned lock, unlike every registered waiter on the same `RawMutex`.
    fn poll_until_value_changes(&self, val: u32, timeout: Option<Duration>) -> UnblockedOrTimedOut {
        let deadline = timeout.map(|t| std::time::Instant::now() + t);
        let mut last_liveness_check = std::time::Instant::now();
        loop {
            if self.inner.load(Ordering::SeqCst) != val {
                return UnblockedOrTimedOut::Unblocked;
            }
            if let Some(deadline) = deadline {
                if std::time::Instant::now() >= deadline {
                    return UnblockedOrTimedOut::TimedOut;
                }
            }
            let now = std::time::Instant::now();
            if now.duration_since(last_liveness_check) >= LIVENESS_CHECK_INTERVAL {
                last_liveness_check = now;
                if self.try_recover_from_dead_holder_unregistered(val) {
                    return UnblockedOrTimedOut::Unblocked;
                }
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }
}

impl Drop for RawMutex {
    fn drop(&mut self) {
        // Close any cross-process handle duplications this mutex's wake path ever cached for
        // itself, and drop the process-local cache rows so they cannot outlive the `RawMutex`
        // instance (its address, part of the cache key, could otherwise be reused by a future
        // allocation and produce a false cache hit for an unrelated mutex). See
        // `REMOTE_WAITER_HANDLES`'s doc comment for why this cache is a process-local `static`
        // rather than a field of `RawMutex` itself.
        let self_addr = core::ptr::from_ref(self) as usize;
        if let Some(cache) = REMOTE_WAITER_HANDLES.get() {
            let mut cache = cache.lock().unwrap();
            let mut i = 0;
            while i < cache.len() {
                if cache[i].0 == self_addr {
                    let (_, _, handle) = cache.swap_remove(i);
                    // SAFETY: `handle` was returned by a `DuplicateHandle` call this same mutex
                    // made into this process's own handle table, and is used nowhere else.
                    unsafe {
                        Win32_Foundation::CloseHandle(handle as Win32_Foundation::HANDLE);
                    }
                } else {
                    i += 1;
                }
            }
        }
    }
}

/// Whether `LITEBOX_DIAG_WAIT_DUR=1` wait-duration diagnostics are enabled. Cached per thread,
/// same pattern as [`diag_rip0_enabled`].
fn diag_wait_dur_enabled() -> bool {
    thread_local! {
        static ENABLED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    }
    ENABLED.with(|e| {
        if let Some(v) = e.get() {
            return v;
        }
        let v = std::env::var_os("LITEBOX_DIAG_WAIT_DUR").is_some();
        e.set(Some(v));
        v
    })
}

/// Whether `LITEBOX_DIAG_MM=1` memory-management diagnostics are enabled. Cached per thread,
/// same pattern as [`diag_wait_dur_enabled`].
///
/// Gates the `diag-commit`/`diag-reclaim`/`diag-decommit`/`diag-vprotect`/`diag-shm` family of
/// `error!`-level log lines that earlier debugging passes added directly to the hot
/// commit/decommit/protect/shared-memory paths with NO env-var gate at all (unlike every other
/// diagnostic in this file). Measured directly (AGENTS.md, "client startup is slow"
/// investigation): a single XFCE client-startup repro emitted 11,405 of these lines in the
/// first 8 seconds of guest execution, before Xwayland was even ready -- every `VirtualAlloc2`/
/// `VirtualProtect`/`VirtualFree`/shared-memory call in the whole run synchronously formats and
/// writes a structured log line, unconditionally, regardless of `LITEBOX_LOG` level (these are
/// `error!` calls, so `LITEBOX_LOG=error` does not suppress them either). A desktop launch
/// performs tens of thousands of such operations across weston/Xwayland/every XFCE component, so
/// this was pure always-on overhead on the single hottest code path in the runtime -- almost
/// certainly a real, previously-unidentified contributor to "client startup takes 60+ seconds
/// with no single explaining stall." Default is now OFF; set `LITEBOX_DIAG_MM=1` to restore the
/// old always-on behavior for a future investigation that specifically needs it.
fn diag_mm_enabled() -> bool {
    thread_local! {
        static ENABLED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    }
    ENABLED.with(|e| {
        if let Some(v) = e.get() {
            return v;
        }
        let v = std::env::var_os("LITEBOX_DIAG_MM").is_some();
        e.set(Some(v));
        v
    })
}

impl litebox::platform::RawMutex for RawMutex {
    const INIT: Self = Self::new();

    fn underlying_atomic(&self) -> &AtomicU32 {
        &self.inner
    }

    fn wake_many(&self, n: usize) -> usize {
        assert!(n > 0, "wake_many should be called with n > 0");

        // Pop up to `n` waiters under `self.waiters`' lock -- the same lock
        // `block_or_maybe_timeout` re-checks on a timeout race -- so a popped record is always
        // exactly the set this call commits to signaling below; nothing else can also claim it.
        let popped: Vec<WaiterRecord> = self.waiters.with_lock(|queue| queue.drain_locked(n));

        let mut woken = 0;
        for record in popped {
            // `resolve_waiter_event` returns `None` only when `record`'s owning process is
            // already gone (see its doc comment) -- there is genuinely nothing left to wake in
            // that case, so this loop skips it and moves on to the rest of the popped set rather
            // than aborting (the earlier `assert!`-based version's lost-wakeup bug: one bad
            // resolution used to panic the WHOLE wake, leaving every other already-popped waiter,
            // which may have been perfectly resolvable, unsignaled forever too).
            let Some(handle) = self.resolve_waiter_event(record) else {
                continue;
            };
            // SAFETY: `handle` is a valid, live auto-reset event handle -- either this thread's
            // direct view of the waiter's own handle (same process) or a handle
            // `resolve_waiter_event` duplicated (or had already cached) from the waiter's own,
            // still-live process. Either way the waiter registered `record` in `self.waiters`
            // before blocking on it and cannot have closed it since: a blocked thread does not
            // run concurrently with the wake that unblocks it, and the drain above already
            // removed `record` under the same lock `block_or_maybe_timeout`'s timeout-race path
            // re-checks, so no other caller can also signal or reclaim it.
            unsafe {
                Win32_Threading::SetEvent(handle);
            }
            woken += 1;
        }

        // Unlike `WakeByAddressSingle`/`WakeByAddressAll`, this manual queue DOES know exactly
        // how many waiters it just signaled, so it reports the real count rather than the
        // conservative `0` the OS primitive forced before this change. Trait contract
        // (`litebox::platform::RawMutex::wake_many`) explicitly allows either.
        woken
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

    fn note_locked(&self) {
        // SAFETY: reading the calling thread's own process id; no preconditions.
        let pid = unsafe { Win32_Threading::GetCurrentProcessId() };
        self.holder_pid.store(pid, Ordering::Release);
    }

    fn note_unlocked(&self) {
        self.holder_pid.store(0, Ordering::Release);
    }

    fn take_poison(&self) -> bool {
        // Method resolution prefers the inherent `take_poison` (defined alongside `poisoned` and
        // `try_recover_from_dead_holder_unregistered`) over this trait method of the same name, so
        // `self.take_poison()` here reaches that inherent one rather than recursing into this trait
        // impl -- kept as a real method there rather than inlined here so it stays next to the
        // field/recovery logic it reads.
        self.take_poison()
    }
}

impl litebox::platform::IPInterfaceProvider for WindowsUserland {
    fn send_ip_packet(&self, packet: &[u8]) -> Result<(), litebox::platform::SendError> {
        net::send_ip_packet(&self.net_gateway, packet)
    }

    fn receive_ip_packet(
        &self,
        packet: &mut [u8],
    ) -> Result<usize, litebox::platform::ReceiveError> {
        net::receive_ip_packet(&self.net_gateway, packet)
    }
}

impl WindowsUserland {
    /// Wait until there is data available from the userspace NAT gateway (see the private `net`
    /// module), or `timeout` elapses. Mirrors `LinuxUserland::wait_on_tun`; used by a
    /// network-worker thread to sleep efficiently between rounds of network interaction instead
    /// of busy-polling.
    pub fn wait_on_tun(&self, timeout: Option<Duration>) {
        net::wait_on_tun(&self.net_gateway, timeout);
    }
}

impl litebox::platform::TimeProvider for WindowsUserland {
    type Instant = Instant;
    type SystemTime = SystemTime;

    fn now(&self) -> Self::Instant {
        let mut ts = 0;
        unsafe { QueryUnbiasedInterruptTimePrecise(&raw mut ts) };
        Instant(ts)
    }

    fn current_time(&self) -> Self::SystemTime {
        let mut filetime = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        unsafe {
            GetSystemTimePreciseAsFileTime(&raw mut filetime);
        }
        let FILETIME {
            dwLowDateTime: low,
            dwHighDateTime: high,
        } = filetime;
        let filetime = (u64::from(high) << 32) | u64::from(low);
        SystemTime { filetime }
    }
}

/// 100ns units returned by `QueryUnbiasedInterruptTimePrecise`.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant(u64);

impl litebox::platform::Instant for Instant {
    fn checked_duration_since(&self, earlier: &Self) -> Option<core::time::Duration> {
        let diff = self.0.checked_sub(earlier.0)?;
        // Convert from 100ns intervals to nanoseconds. This won't overflow in
        // our lifetimes.
        Some(Duration::from_nanos(diff * 100))
    }

    fn checked_add(&self, duration: core::time::Duration) -> Option<Self> {
        let duration_100ns: u64 = (duration.as_nanos() / 100).try_into().ok()?;
        let new = self.0.checked_add(duration_100ns)?;
        Some(Instant(new))
    }
}

pub struct SystemTime {
    // 100ns intervals since Windows epoch
    filetime: u64,
}

impl litebox::platform::SystemTime for SystemTime {
    // Windows epoch: Jan 1, 1601
    // Unix epoch: Jan 1, 1970
    // Difference: 11644473600 seconds
    // Intervals: 100ns intervals
    // Seconds per interval: 10^-7
    const UNIX_EPOCH: Self = SystemTime {
        filetime: 11_644_473_600 * 10_000_000,
    };

    fn duration_since(&self, earlier: &Self) -> Result<core::time::Duration, core::time::Duration> {
        if self.filetime >= earlier.filetime {
            let diff_100ns = self.filetime - earlier.filetime;
            let nanos = diff_100ns * 100;
            let secs = nanos / 1_000_000_000;
            let remaining_nanos = nanos % 1_000_000_000;
            Ok(core::time::Duration::new(secs, remaining_nanos as u32))
        } else {
            let diff_100ns = earlier.filetime - self.filetime;
            let nanos = diff_100ns * 100;
            let secs = nanos / 1_000_000_000;
            let remaining_nanos = nanos % 1_000_000_000;
            Err(core::time::Duration::new(secs, remaining_nanos as u32))
        }
    }
}

impl litebox::platform::ArchSpecificProvider for WindowsUserland {
    fn set_arch_specific_register(
        &self,
        reg: &litebox::platform::ArchSpecificRegister,
        val: usize,
    ) -> Result<(), litebox::platform::ArchSpecificError> {
        match reg {
            litebox::platform::ArchSpecificRegister::FsBase => {
                // Two checks, deliberately layered: `is_valid_user_fs_base` enforces the generic
                // x86_64 Linux ABI ceiling (`USER_ADDR_END`), which is NOT tight enough on this
                // platform specifically -- this platform's actual guest-addressable ceiling
                // (`TASK_ADDR_MAX`) sits far below `USER_ADDR_END`, and everything from
                // `TASK_ADDR_MAX` up through `HOST_ALLOCATOR_REGION_MIN`'s reserved 64 GiB span
                // belongs to the HOST process (its own stack, modules, and the host global
                // allocator's own reserved region -- see `HOST_ALLOCATOR_REGION_MIN`'s doc
                // comment), never to the guest. `arch_prctl(ARCH_SET_FS)` and `clone(CLONE_SETTLS)`
                // both funnel through this one function, so rejecting an out-of-range value here
                // closes off, at that chokepoint, the musl dtv-clear crash this investigation has
                // chased for many passes (`mov rdx, [rax+0x80]` on a `rax` proven to be a live
                // `HOST_ALLOCATOR_REGION_MIN`-range address) for those two callers. It does NOT
                // cover `fork()`'s own parent-to-child FS-base propagation, which does NOT funnel
                // through this function -- see the three actual fork-path sites instead:
                // `litebox_platform_windows_userland::process_fork::deserialize_full_gprs` (parses
                // `fs_base` off a cross-process text protocol with no validation of its own),
                // `litebox_runner_linux_on_windows_userland`'s `diag_process_fork_task_resume_probe`
                // (applies `gprs.fs_base` to the freshly-spawned child -- routed through THIS
                // function, so it inherits this validation and fails closed via `.expect(..)` on
                // rejection, panicking only the fresh child process), and
                // `litebox_shim_linux::syscalls::process`'s `do_clone` (computes
                // `cross_process_fs_base` from this platform's own already-validated live FS base
                // before handing it to `spawn_cross_process_fork_child`). Each of those sites either
                // routes through this function already or is fed a value this function already
                // validated when it was first set -- but they are validated INDIRECTLY, not by a
                // second direct call at the fork boundary itself, so a defect anywhere in that
                // chain (e.g. the raw, diagnostic-only `SetThreadContext` injection in
                // `real_resume_and_observe`, gated off by default and never used to resume a child
                // in production) would not be caught here. Diagnostic rejection logging
                // (`diag_raw_print`) is emitted below so a future pass can tell, from hard evidence,
                // whether this chokepoint or the fork-path sites are where a corrupted value is
                // actually being produced.
                let task_addr_max = <Self as litebox::platform::PageManagementProvider<
                    { litebox::mm::linux::PAGE_SIZE },
                >>::TASK_ADDR_MAX;
                if litebox_common_linux::arch::is_valid_user_fs_base(val) && val < task_addr_max {
                    // Use WindowsUserland's per-thread FS base management system
                    Self::set_thread_fs_base(val);
                    Ok(())
                } else {
                    diag_raw_print(
                        b"[fsbase-reject] set_arch_specific_register: val=0x",
                        val,
                        b" task_addr_max=0x",
                        task_addr_max,
                    );
                    Err(litebox::platform::ArchSpecificError::RegisterUnpermittedValue)
                }
            }
            litebox::platform::ArchSpecificRegister::GsBase => {
                // Windows uses GS for its own thread environment block
                // (TEB); the host platform does not expose a safe way for
                // the guest to program gs base without breaking the host.
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
            litebox::platform::ArchSpecificRegister::FsBase => Ok(Self::get_thread_fs_base()),
            litebox::platform::ArchSpecificRegister::GsBase => {
                // See note above: gs base is reserved by the Windows host.
                Err(litebox::platform::ArchSpecificError::RegisterReserved)
            }
            _ => Err(litebox::platform::ArchSpecificError::RegisterUnsupported),
        }
    }

    /// Writes the guest's live xmm0-xmm15 (256 bytes) into `out[..256]`.
    ///
    /// Correctness depends on the same invariant `guest_xmm0_5` itself relies on (see that
    /// field's doc comment): whenever host Rust code is running (which this function's own
    /// caller always is, since it's a plain Rust method, never reached from the naked-asm guest
    /// entry trampolines directly), xmm0-xmm5 are the Windows x64 ABI's *caller-saved* registers
    /// -- any host code between a guest exit and this call is free to have clobbered them, so the
    /// only reliably-guest-valid copy is the one `syscall_callback` already captured into
    /// `TlsState.guest_xmm0_5` before any other host code ran. xmm6-xmm15 are *callee*-saved by
    /// the same ABI, and `run_thread_arch`'s own prologue/epilogue already preserves them for the
    /// guest-thread's *entire* lifetime around all host code -- so those six-through-fifteen are
    /// still genuinely live, guest-valid values in the real hardware registers right now, with no
    /// separate capture needed.
    fn get_fp_state(&self, out: &mut [u8]) -> Result<(), litebox::platform::ArchSpecificError> {
        const XMM_BYTES: usize = 16 * 16;
        if out.len() < XMM_BYTES {
            return Err(litebox::platform::ArchSpecificError::RegisterUnpermittedValue);
        }
        let Some(tls) = get_tls_ptr() else {
            return Err(litebox::platform::ArchSpecificError::RegisterUnsupported);
        };
        let tls = unsafe { &*tls };
        out[0..96].copy_from_slice(tls.guest_xmm0_5.get().as_bytes());
        let p = out.as_mut_ptr();
        unsafe {
            core::arch::asm!(
                "movups [{p} + 6*16], xmm6",
                "movups [{p} + 7*16], xmm7",
                "movups [{p} + 8*16], xmm8",
                "movups [{p} + 9*16], xmm9",
                "movups [{p} + 10*16], xmm10",
                "movups [{p} + 11*16], xmm11",
                "movups [{p} + 12*16], xmm12",
                "movups [{p} + 13*16], xmm13",
                "movups [{p} + 14*16], xmm14",
                "movups [{p} + 15*16], xmm15",
                p = in(reg) p,
                options(nostack),
            );
        }
        Ok(())
    }

    /// Writes `state[..256]` back into the guest's real xmm0-xmm15, the inverse of
    /// [`Self::get_fp_state`]. See that function's doc comment for why xmm0-5 must go through
    /// `TlsState.guest_xmm0_5` (restored to the live registers later, at the same point
    /// `switch_to_guest`'s existing xmm0-5 restore already runs) rather than being written to the
    /// hardware registers directly here: this call's own caller is host Rust code, so any xmm0-5
    /// value written straight to hardware now would just be clobbered by the next caller-saved
    /// use before the guest ever resumes.
    fn set_fp_state(&self, state: &[u8]) -> Result<(), litebox::platform::ArchSpecificError> {
        const XMM_BYTES: usize = 16 * 16;
        if state.len() < XMM_BYTES {
            return Err(litebox::platform::ArchSpecificError::RegisterUnpermittedValue);
        }
        let Some(tls) = get_tls_ptr() else {
            return Err(litebox::platform::ArchSpecificError::RegisterUnsupported);
        };
        let tls = unsafe { &*tls };
        let mut xmm0_5 = [0u128; 6];
        xmm0_5.as_mut_bytes().copy_from_slice(&state[0..96]);
        tls.guest_xmm0_5.set(xmm0_5);
        let p = state.as_ptr();
        unsafe {
            core::arch::asm!(
                "movups xmm6, [{p} + 6*16]",
                "movups xmm7, [{p} + 7*16]",
                "movups xmm8, [{p} + 8*16]",
                "movups xmm9, [{p} + 9*16]",
                "movups xmm10, [{p} + 10*16]",
                "movups xmm11, [{p} + 11*16]",
                "movups xmm12, [{p} + 12*16]",
                "movups xmm13, [{p} + 13*16]",
                "movups xmm14, [{p} + 14*16]",
                "movups xmm15, [{p} + 15*16]",
                p = in(reg) p,
                out("xmm6") _, out("xmm7") _, out("xmm8") _, out("xmm9") _, out("xmm10") _,
                out("xmm11") _, out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
                options(nostack, readonly),
            );
        }
        Ok(())
    }
}

type UserConstPtr<T> = litebox::platform::common_providers::userspace_pointers::UserConstPtr<
    litebox::platform::common_providers::userspace_pointers::NoValidation,
    T,
>;
type UserMutPtr<T> = litebox::platform::common_providers::userspace_pointers::UserMutPtr<
    litebox::platform::common_providers::userspace_pointers::NoValidation,
    T,
>;

impl litebox::platform::RawPointerProvider for WindowsUserland {
    type RawConstPointer<T: FromBytes> = UserConstPtr<T>;
    type RawMutPointer<T: FromBytes + IntoBytes> = UserMutPtr<T>;
}

#[allow(
    clippy::match_same_arms,
    reason = "Iterate over all cases for prot_flags."
)]
fn prot_flags(flags: MemoryRegionPermissions) -> Win32_Memory::PAGE_PROTECTION_FLAGS {
    match (
        flags.contains(MemoryRegionPermissions::READ),
        flags.contains(MemoryRegionPermissions::WRITE),
        flags.contains(MemoryRegionPermissions::EXEC),
    ) {
        // no permissions
        (false, false, false) => Win32_Memory::PAGE_NOACCESS,
        // read-only
        (true, false, false) => Win32_Memory::PAGE_READONLY,
        // write-only (Windows doesn't have write-only, so we use r+w)
        (false, true, false) => Win32_Memory::PAGE_READWRITE,
        // read-write
        (true, true, false) => Win32_Memory::PAGE_READWRITE,
        // exeute-only (Windows doesn't have execute-only, so we use r+x)
        (false, false, true) => Win32_Memory::PAGE_EXECUTE_READ,
        // read-execute
        (true, false, true) => Win32_Memory::PAGE_EXECUTE_READ,
        // write-execute (Windows doesn't have write-execute, so we use rwx)
        (false, true, true) => Win32_Memory::PAGE_EXECUTE_READWRITE,
        // read-write-execute
        (true, true, true) => Win32_Memory::PAGE_EXECUTE_READWRITE,
    }
}

fn do_prefetch_on_range(start: usize, size: usize) {
    let ok = unsafe {
        let prefetch_entry = Win32_Memory::WIN32_MEMORY_RANGE_ENTRY {
            VirtualAddress: start as *mut c_void,
            NumberOfBytes: size,
        };
        PrefetchVirtualMemory(GetCurrentProcess(), 1, &raw const prefetch_entry, 0) != 0
    };
    assert!(ok, "PrefetchVirtualMemory failed with error: {}", unsafe {
        GetLastError()
    });
}

fn do_query_on_region(mbi: &mut Win32_Memory::MEMORY_BASIC_INFORMATION, base_addr: *mut c_void) {
    let ok = unsafe {
        Win32_Memory::VirtualQuery(
            base_addr,
            mbi,
            core::mem::size_of::<Win32_Memory::MEMORY_BASIC_INFORMATION>(),
        ) != 0
    };
    assert!(ok, "VirtualQuery addr={:p} failed: {}", base_addr, unsafe {
        GetLastError()
    });
}

/// Helper method to process a memory range by iterating through Windows memory regions.
///
/// Windows memory is managed in Virtual Address Descriptors (VADs) at the NT kernel level,
/// which means a single user-space range might span multiple regions. This helper method
/// queries each region within the specified range and applies the given operation.
///
/// # Parameters
/// - `range`: The memory range to process
/// - `operation`: A closure that takes (region_range, region_state) and returns Result<bool, E>.
///
/// # Panics
///
/// Panics if the operation returns false for any region.
fn process_memory_range_by_regions<F, E>(
    mut range: core::ops::Range<usize>,
    mut operation: F,
) -> Result<(), E>
where
    F: FnMut(core::ops::Range<usize>, Win32_Memory::VIRTUAL_ALLOCATION_TYPE) -> Result<bool, E>,
{
    while !range.is_empty() {
        let mut mbi = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
        do_query_on_region(&mut mbi, range.start as *mut c_void);
        // `VirtualQuery` returns the region CONTAINING `range.start`, not necessarily one that
        // BEGINS at `range.start` -- `range.start` can legitimately fall mid-region (e.g. a
        // `munmap()`/`mprotect()` of a sub-range of a larger VAD node Windows hasn't split yet).
        // The now-removed `debug_assert_eq!(range.start, mbi.BaseAddress)` assumed this could
        // never happen and was compiled out entirely in release builds, silently hiding it rather
        // than catching it -- and even in a debug build that only aborts loudly, it never fixed
        // the actual bug below: `mbi.RegionSize` is measured from `mbi.BaseAddress`, not from
        // `range.start`, so when the two diverge (`mbi.BaseAddress < range.start`), the OLD
        // `len = mbi.RegionSize.min(range.len())` used the region's FULL size measured from its
        // own earlier base -- overshooting past the region's real boundary (relative to
        // `range.start`) by `range.start - mbi.BaseAddress` bytes, extending `len` into
        // WHATEVER memory sits immediately after this region, decommitting/reprotecting/etc.
        // pages the caller never asked to touch at all. This is exactly the shape of bug that
        // would silently corrupt a neighboring allocation's memory right after an ordinary
        // `munmap()` returns success -- the guest's own next access to that neighboring
        // allocation then faults with no apparent cause, since nothing about the fault itself
        // points back to an unrelated `munmap()` call that already returned and moved on.
        let region_end_from_query_base = mbi.BaseAddress as usize + mbi.RegionSize;
        let region_remaining_from_range_start = region_end_from_query_base.saturating_sub(range.start);
        let len = region_remaining_from_range_start.min(range.len());
        debug_assert!(
            len > 0,
            "process_memory_range_by_regions: computed a zero-length operation at {:p} \
             (query base={:p}, region_size={:#x}) -- would loop forever",
            range.start as *mut c_void,
            mbi.BaseAddress,
            mbi.RegionSize
        );
        let success = operation(range.start..range.start + len, mbi.State)?;
        if !success {
            litebox_util_log::error!(
                start:% = range.start,
                end:% = range.start + len,
                mbi_state:% = mbi.State,
                mbi_type:% = mbi.Type,
                mbi_protect:% = mbi.Protect,
                mbi_alloc_protect:% = mbi.AllocationProtect,
                mbi_base:% = mbi.BaseAddress as usize,
                mbi_region_size:% = mbi.RegionSize,
                last_error:% = std::io::Error::last_os_error();
                "diag-region-op-fail: operation failed on region"
            );
        }
        assert!(
            success,
            "operation failed on region {:p}-{:p}: {}",
            range.start as *mut c_void,
            (range.start + len) as *mut c_void,
            std::io::Error::last_os_error()
        );
        range = (range.start + len)..range.end;
    }
    Ok(())
}

macro_rules! debug_assert_alignment {
    ($r:ident, $page_size:expr) => {
        debug_assert!($r.start.is_multiple_of($page_size));
        debug_assert!($r.end.is_multiple_of($page_size));
    };
}

/// Lower boundary (inclusive) of the address range exclusively reserved for the host-side
/// global Rust allocator ([`SLAB_ALLOC`]/[`WindowsUserland::alloc`]). Nothing below this
/// address is ever requested by [`WindowsUserland::alloc`], and the guest's own
/// [`litebox::platform::PageManagementProvider::TASK_ADDR_MAX`] is set to end strictly below
/// it (see that constant's own doc comment). This is a fixed split of the process's 47-bit
/// canonical user address space (`0`..`0x7FFF_FFFF_FFFF`), carving off the top 64 GiB for the
/// host allocator and leaving the rest for the guest. 64 GiB (not a smaller value) because the
/// host allocator backs ordinary host-process `Vec`/`String` growth too -- e.g. reading a
/// multi-gigabyte initial-files tar archive via `std::fs::read` -- and a too-small reservation
/// reintroduces a real, easily-hit out-of-memory failure (confirmed live: a 2 GiB reservation
/// failed to read a 1.3 GB tar file, since `SafeZoneAllocator` doubles requested size for
/// alignment padding and its buddy allocator's largest block class is `1 << ORDER` = 16 GiB).
///
/// # Why this exists
///
/// Before this split existed, [`WindowsUserland::alloc`] called `VirtualAlloc2` with a null
/// (OS-chosen) base address, completely unconstrained. Windows was therefore free to hand back
/// an address anywhere in the process's address space, including inside
/// `TASK_ADDR_MIN..TASK_ADDR_MAX`, the same range the guest's own `Vmem` independently manages
/// for guest mmap/heap/stack placement. `Vmem`'s own placement logic only avoids addresses
/// captured in a one-time startup snapshot (`WindowsUserland::read_memory_maps`), so it had no
/// visibility into memory the global allocator committed later during ordinary guest execution.
/// When both sides raced to commit overlapping virtual addresses, Windows gave no error --
/// each `VirtualAlloc2` call "succeeds" independently -- and whichever side committed second
/// silently aliased memory the other side believed it exclusively owned, corrupting guest state
/// (e.g. musl's malloc/TLS bookkeeping) with unrelated host allocator content. Statically
/// partitioning the address space so the two allocators' domains are disjoint makes that
/// collision unrepresentable rather than merely unlikely.
const HOST_ALLOCATOR_REGION_MIN: usize = 0x7FF0_0000_0000;

impl<const ALIGN: usize> litebox::platform::PageManagementProvider<ALIGN> for WindowsUserland {
    // TODO(chuqi): These are currently "magic numbers" grabbed from my Windows 11 SystemInformation.
    // The actual values should be determined by `GetSystemInfo()`.
    //
    // NOTE: make sure the values are PAGE_ALIGNED.
    const TASK_ADDR_MIN: usize = 0x1_0000;
    /// Kept strictly below [`HOST_ALLOCATOR_REGION_MIN`] so the guest's own `Vmem` placement
    /// (which only avoids a one-time startup snapshot of committed regions, see that constant's
    /// doc comment) can never be handed an address the host global allocator later claims.
    const TASK_ADDR_MAX: usize = HOST_ALLOCATOR_REGION_MIN - 0x1_0000;
    fn allocate_pages(
        &self,
        suggested_range: core::ops::Range<usize>,
        initial_permissions: MemoryRegionPermissions,
        can_grow_down: bool,
        populate_pages_immediately: bool,
        fixed_address_behavior: FixedAddressBehavior,
    ) -> Result<Self::RawMutPointer<u8>, AllocationError> {
        debug_assert!(ALIGN.is_multiple_of(self.sys_info.read().unwrap().dwPageSize as usize));
        debug_assert_alignment!(suggested_range, ALIGN);

        // DIAG (AGENTS.md pass 227): unconditional print of the actual `fixed_address_behavior`
        // this call receives, plus the requested range -- pass 226 proved a `Replace`-mode
        // fixed request IS being silently relocated somewhere in this function despite
        // `found:false` on the foreign-claim check, and this investigation's working assumption
        // (unverified until now) has been that every plain `MAP_FIXED` call reaches this
        // function tagged `Replace`. Confirm or refute that assumption directly.
        if suggested_range.start != 0 {
            litebox_util_log::debug!(
                start:% = suggested_range.start, end:% = suggested_range.end,
                behavior:? = fixed_address_behavior;
                "DIAG allocate_pages: entry, fixed-addr call"
            );
        }

        // A helper closure to reserve and commit memory in one go.
        //
        // Note that MEM_RESERVE requires the base address to be aligned to system allocation granularity,
        // while MEM_COMMIT only requires page-aligned address.
        //
        // To ensure future MEM_COMMIT calls on sub-ranges succeed, we always reserve the entire aligned range
        // (i.e., MEM_RESERVE size is also made aligned to system allocation granularity).
        //
        // This deliberately leaves up to `dwAllocationGranularity - 1` bytes on each side of the
        // caller's unrounded `r` reserved but never committed (PRD
        // `windows-reserve-and-commit-64kib-granularity-noaccess-flanks`). Do NOT "fix" this by
        // widening MEM_COMMIT to the full aligned span or by shrinking MEM_RESERVE to `r`: the
        // former would commit real memory into a granule another, unrelated allocation may later
        // legitimately claim (see the flank-restoration note below and `allocate_pages`'s
        // whole-view CoW reservation, both of which rely on neighbours sharing a granule), and
        // the latter is not achievable at all -- Windows rejects a MEM_RESERVE base that is not
        // itself granularity-aligned. The asymmetry is already the intended guard, not a gap:
        // reserved-but-uncommitted memory is unbacked, so any guest access landing in a flank
        // faults exactly like a real out-of-bounds access would, before ever reaching
        // `change_page_permissions`'s tracked range. Live-verified with a standalone
        // VirtualAlloc/VirtualQuery probe replicating this exact shape (reserve a 3-granule span
        // at PAGE_NOACCESS, commit only a deliberately unaligned inner sub-range): both flanks
        // report State=MEM_RESERVE while the sub-range reports MEM_COMMIT/PAGE_READWRITE, and
        // reading one byte from a flank raises an uncatchable native access violation (not a
        // recoverable managed exception), confirming the flank is a hard fault boundary, not
        // silently-readable memory. The only real cost is a few KiB of otherwise-unusable
        // reserved (never committed, never charged) address space per allocation, negligible on
        // a 64-bit process's address space.
        // `floor` raises `MEM_ADDRESS_REQUIREMENTS::LowestStartingAddress` for the
        // OS-picks-the-address (`r.start == 0`) path. Windows satisfies an unconstrained
        // request BOTTOM-UP from the lowest free address, so a caller that had a perfectly
        // good high address and merely lost it to a foreign-claim collision would otherwise
        // be relocated to the very bottom of the guest range -- see the `hint_foreign_claim`
        // fallback below for the packing that causes. Passing the discarded hint as `floor`
        // keeps the retry in the same neighbourhood instead.
        let reserve_and_commit = |r: core::ops::Range<usize>,
                                  flags: Win32_Memory::PAGE_PROTECTION_FLAGS,
                                  floor: usize|
         -> *mut c_void {
            let aligned_start_addr = self.round_down_to_granu(r.start);
            let aligned_end_addr = self.round_up_to_granu(r.end);

            // When the caller (guest `Vmem`) does not suggest a specific address (`r.start ==
            // 0`), the MEM_RESERVE below previously asked Windows to pick ANY free address in
            // the whole process with no upper bound -- unlike every OTHER guest allocation path
            // in this function, which is asserted to stay within `TASK_ADDR_MIN..TASK_ADDR_MAX`
            // (see the `suggested_range.start != 0` branch below). Windows is free to satisfy an
            // unconstrained request from anywhere, including inside
            // `HOST_ALLOCATOR_REGION_MIN..`, the range this process's own global allocator
            // (`WindowsUserland::alloc`, below) is exclusively constrained to via the mirror-image
            // `MEM_ADDRESS_REQUIREMENTS` there. A guest allocation landing in that region is
            // indistinguishable, from the guest's perspective, from ordinary guest heap memory --
            // it silently aliases whatever the host allocator later places at the same address,
            // corrupting both sides with no page fault to signal it. Constrain this path
            // symmetrically to `TASK_ADDR_MIN..TASK_ADDR_MAX` so the two regions can never
            // overlap.
            let lowest_start = core::cmp::max(
                <WindowsUserland as litebox::platform::PageManagementProvider<ALIGN>>::TASK_ADDR_MIN,
                self.round_down_to_granu(floor),
            );
            let mut addr_req = MEM_ADDRESS_REQUIREMENTS {
                LowestStartingAddress: lowest_start as *mut c_void,
                HighestEndingAddress: (<WindowsUserland as litebox::platform::PageManagementProvider<ALIGN>>::TASK_ADDR_MAX - 1) as *mut c_void,
                Alignment: 0,
            };
            let mut ext_param = MEM_EXTENDED_PARAMETER {
                Anonymous1: MEM_EXTENDED_PARAMETER_0 {
                    _bitfield: MemExtendedParameterAddressRequirements as u64,
                },
                Anonymous2: windows_sys::Win32::System::Memory::MEM_EXTENDED_PARAMETER_1 {
                    Pointer: (&raw mut addr_req).cast::<c_void>(),
                },
            };
            let ptr = if r.start == 0 {
                unsafe {
                    VirtualAlloc2(
                        GetCurrentProcess(),
                        core::ptr::null_mut(),
                        aligned_end_addr - aligned_start_addr,
                        Win32_Memory::MEM_RESERVE,
                        Win32_Memory::PAGE_NOACCESS,
                        &raw mut ext_param,
                        1,
                    )
                }
            } else {
                unsafe {
                    VirtualAlloc2(
                        GetCurrentProcess(),
                        aligned_start_addr as *mut c_void,
                        aligned_end_addr - aligned_start_addr,
                        Win32_Memory::MEM_RESERVE,
                        Win32_Memory::PAGE_NOACCESS,
                        core::ptr::null_mut(),
                        0,
                    )
                }
            };
            // A failed `MEM_RESERVE` at an EXPLICIT address is not necessarily fatal: the
            // granule may already be reserved, by us. Windows reservations are always
            // granularity-granular, so two mappings whose pages share one 64 KiB granule
            // CANNOT hold separate reservations -- they must live inside the same one. The
            // reservation above is rounded out to granularity precisely so it can serve
            // arbitrary page-aligned addresses, which means it routinely collides with a granule
            // some neighbouring mapping already owns. In particular the whole-view reservation
            // that restores a destroyed CoW view's flanks (see `allocate_pages`) deliberately
            // holds entire granules that later, unrelated `mmap`s will land in.
            //
            // Windows reports that collision as `ERROR_INVALID_ADDRESS`, the same code it uses
            // for a genuinely unusable address, so the two cannot be told apart from the error
            // alone. Distinguish them by simply attempting the commit: if the address space is
            // already ours, `MEM_COMMIT` succeeds; if it is not, the commit fails and this
            // returns null exactly as it did before.
            //
            // This is load-bearing for the flank restoration, not a tidy-up: without it,
            // restoring a flank correctly BREAKS the next allocation that rounds into the same
            // granule, turning a latent SIGSEGV into an immediate panic.
            let maybe_already_reserved = ptr.is_null()
                && r.start != 0
                && unsafe { GetLastError() }
                    == windows_sys::Win32::Foundation::ERROR_INVALID_ADDRESS;
            if ptr.is_null() && !maybe_already_reserved {
                core::ptr::null_mut()
            } else {
                let commit_addr = if r.start == 0 { ptr } else { r.start as *mut c_void };
                if diag_mm_enabled() {
                    litebox_util_log::debug!(
                        start:% = commit_addr as usize, end:% = commit_addr as usize + r.len(),
                        len:% = r.len(), pid:% = std::process::id(),
                        tid:? = std::thread::current().id();
                        "diag-commit: VirtualAlloc2(MEM_COMMIT) reserve_and_commit"
                    );
                }
                unsafe {
                    VirtualAlloc2(
                        GetCurrentProcess(),
                        commit_addr,
                        r.len(),
                        Win32_Memory::MEM_COMMIT,
                        flags,
                        core::ptr::null_mut(),
                        0,
                    )
                }
            }
        };

        let mut base_addr = suggested_range.start as *mut c_void;
        // See the `hint_foreign_claim` fallback below: set to the discarded hint so the
        // OS-picks-the-address retry stays in the same region instead of restarting from
        // `TASK_ADDR_MIN`. Zero means "no preference" (the ordinary `mmap(NULL, ...)` case).
        let mut placement_floor = 0usize;
        let size = suggested_range.len();
        // TODO: For Windows, there is no MAP_GROWDOWN features so far.
        let _ = can_grow_down;

        if suggested_range.start != 0 {
            assert!(suggested_range.start >= <WindowsUserland as litebox::platform::PageManagementProvider<ALIGN>>::
                                                            TASK_ADDR_MIN);
            assert!(suggested_range.end <= <WindowsUserland as litebox::platform::PageManagementProvider<ALIGN>>::
                                                            TASK_ADDR_MAX);

            // Hold `ALLOCATE_PAGES_FIXED_ADDR_LOCK` across the ENTIRE check-then-act sequence
            // below (the `has_committed_page` query through the final `VirtualAlloc2`/`VirtualFree`
            // calls) -- without this, a real TOCTOU race exists: `has_committed_page` can observe
            // a range as free, but before this thread's own subsequent allocation call actually
            // reserves/commits it, a DIFFERENT concurrently-running guest process's thread could
            // allocate into the exact same real address (confirmed real and unfixed via direct
            // code review: the existing `assert_eq!(fixed_address_behavior, Replace, "raced with
            // another memory allocator")` a few lines below is a pre-existing, already-known,
            // still-live acknowledgment of this exact gap, previously marked only with a
            // `// TODO: handle this race condition properly` comment). Real Linux's kernel-level
            // `mmap()` is atomic against concurrent sibling-process mmaps; this two-Windows-API-
            // call reimplementation was not, until now. Scoped to only the fixed-address path
            // (`suggested_range.start != 0`) -- the OS-picks-any-address path (`start == 0`) has
            // no equivalent race, since `VirtualAlloc2` with no address hint is itself atomic.
            // Time both the WAIT for this lock and the HOLD of it. This lock is
            // shared by allocate_pages, deallocate_pages, update_permissions and
            // unmap_shared_memory, so a long hold freezes every guest thread that
            // touches memory -- the "all processes silent at once, then all resume"
            // signature seen in the scanout investigation (up to 59.8s of a 117s run).
            // Measuring wait vs hold separates "one slow holder" from "many short
            // acquisitions contending", which need different fixes.
            let adv_wait_start = std::time::Instant::now();
            let _fixed_addr_guard = ALLOCATE_PAGES_FIXED_ADDR_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let adv_waited = adv_wait_start.elapsed();
            let adv_hold_start = std::time::Instant::now();
            struct AdvLockTimer(std::time::Instant, usize, std::time::Duration);
            impl Drop for AdvLockTimer {
                fn drop(&mut self) {
                    let held = self.0.elapsed();
                    // Only report acquisitions that actually cost something, so the
                    // common fast path does not flood the log and skew the run.
                    if held.as_millis() >= 50 || self.2.as_millis() >= 50 {
                        litebox_util_log::debug!(
                            held_ms:% = held.as_millis(),
                            waited_ms:% = self.2.as_millis(),
                            len:% = self.1;
                            "diag-lockhold: ALLOCATE_PAGES_FIXED_ADDR_LOCK"
                        );
                    }
                }
            }
            let _adv_lock_timer = AdvLockTimer(
                adv_hold_start,
                suggested_range.end - suggested_range.start,
                adv_waited,
            );

            let has_committed_page =
                process_memory_range_by_regions(suggested_range.clone(), |r, state| {
                    let mbi_type = {
                        let mut mbi = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
                        do_query_on_region(&mut mbi, r.start as *mut c_void);
                        mbi.Type
                    };
                    litebox_util_log::debug!(
                        start:% = r.start, end:% = r.end, state:? = state, mbi_type:? = mbi_type;
                        "allocate_pages: DIAG region state at has_committed_page check"
                    );
                    if state == Win32_Memory::MEM_COMMIT {
                        Err(())
                    } else {
                        Ok(true)
                    }
                })
                .is_err();
            // AGENTS.md pass 228: `Hint`-mode previously only checked Windows' own real
            // `VirtualQuery`-visible commit state (`has_committed_page`) before deciding whether
            // to relocate -- `CLAIMED_RANGES` (this crate's own litebox-internal ownership
            // registry, tracking guest-process memory Windows itself cannot distinguish from
            // "free", see that static's own doc comment) was never consulted for `Hint`-mode at
            // all, unlike `Replace`/`NoReplace` a few lines below. Root-caused live: a PIE
            // binary's own base-address reservation (`elf.rs`'s `reserve()`, a plain `Hint`-mode
            // call) could pick an address Windows reports as genuinely free but that ALREADY
            // belongs to a different, still-live guest process's own memory (e.g. a long-running
            // shell's own independently-grown heap) -- invisible to `VirtualQuery` alone. The
            // reservation would then succeed at that colliding address, and every subsequent
            // `MAP_FIXED` segment placed relative to it would genuinely, unavoidably collide,
            // surfacing later (correctly, per pass 213's own fix) as `EEXIST` instead of being
            // caught and relocated here where it belongs. Extend the SAME foreign-claim defense
            // `Replace`-mode already has to this path too, so a `Hint`-mode reservation gets a
            // fresh address instead of one already claimed by someone else.
            let hint_foreign_claim = fixed_address_behavior == FixedAddressBehavior::Hint
                && find_foreign_claim(suggested_range.clone(), current_claim_owner()).is_some();
            // See `overlaps_shared_kernel_heap`'s own doc comment: the shared kernel heap's
            // `MEM_RESERVE` fallback view is invisible to both `has_committed_page` (COMMIT-only)
            // and `find_foreign_claim` (never registered in `CLAIMED_RANGES`) without this.
            let hint_shared_heap_hit = fixed_address_behavior == FixedAddressBehavior::Hint
                && overlaps_shared_kernel_heap(&suggested_range);
            if (has_committed_page || hint_foreign_claim || hint_shared_heap_hit)
                && fixed_address_behavior == FixedAddressBehavior::Hint
            {
                // If any page in the suggested range is already committed, and the caller
                // did not request a fixed address, we ask the OS to allocate a new region.
                //
                // Remember the address we are discarding. `get_unmmaped_area` chose it
                // top-down and it is very likely still the right NEIGHBOURHOOD even though
                // this exact range collided; an unconstrained retry, by contrast, is served
                // bottom-up by Windows and lands at the very bottom of the guest range.
                // Confirmed live: a forked Xorg had 45 of 48 allocator-chosen mappings placed
                // low this way (vs 0 of 118 for the same binary as pid 1), packing ~180
                // mappings into 177 MB with 125 of 133 gaps under 64 KB -- several exactly
                // zero -- until glibc's `sysmalloc` grew the heap straight into an adjacent
                // library's text segment and SIGSEGV'd. `MAP_SHARED` was unaffected precisely
                // because `map_shared_memory` never reaches this fallback.
                // `LITEBOX_NO_PLACEMENT_FLOOR=1` disables the floor at RUNTIME, so one binary
                // can be A/B'd with and without this fix. A build-vs-build comparison confounds
                // the fix with every other tree change between the two builds; this does not.
                if std::env::var_os("LITEBOX_NO_PLACEMENT_FLOOR").is_none() {
                    placement_floor = suggested_range.start;
                }
                base_addr = core::ptr::null_mut();
            } else if (has_committed_page || overlaps_shared_kernel_heap(&suggested_range))
                && fixed_address_behavior == FixedAddressBehavior::NoReplace
            {
                return Err(AllocationError::AddressInUse);
            } else if fixed_address_behavior == FixedAddressBehavior::Replace
                && overlaps_shared_kernel_heap(&suggested_range)
            {
                // A genuine `MAP_FIXED` request landing exactly on the shared kernel heap's own
                // live mapping cannot be relocated (that is what `Replace` means) and must never
                // be allowed to silently commit into another subsystem's live section -- fail
                // loudly rather than corrupt it. Astronomically rare in practice (would need a
                // guest fixed-address load to exactly hit this process's own OS-chosen fallback
                // address), unlike the `Hint`-mode case above.
                return Err(AllocationError::AddressInUse);
            } else if fixed_address_behavior == FixedAddressBehavior::Replace
                && {
                    // Checked regardless of `has_committed_page`: a foreign claim can cover a
                    // range Windows currently reports as MEM_FREE or MEM_RESERVE (not yet
                    // MEM_COMMIT) when the owning thread reserved-but-hasn't-yet-committed it, or
                    // when this thread's own view of "committed" raced with the owner's. Only
                    // checking `has_committed_page && Replace` (the prior condition) let a
                    // `Replace`-mode caller's `MEM_FREE`/`MEM_RESERVE` branch below commit
                    // straight over another thread's still-live claim with zero foreign-claim
                    // check at all -- confirmed live via a weston + weston-desktop-shell repro:
                    // one thread's own already-committed `mmap(NULL, 4096)` region was corrupted
                    // by a sibling thread's `brk()`-driven `Replace`-mode growth landing on it,
                    // because `has_committed_page` observed the target range as not-yet-MEM_COMMIT
                    // at the moment this thread queried it, skipping this check entirely under the
                    // old `has_committed_page &&` gate.
                    //
                    // AGENTS.md pass 216: retry this check a few times with a short sleep before
                    // accepting a foreign-claim hit as final. Root-caused (passes 213-215) that a
                    // `Replace`-mode collision here is very often a genuinely transient, both-
                    // still-alive-for-a-moment race between an `ET_EXEC` binary's fixed load
                    // address and a short-lived SIBLING process (e.g. a shell's own just-forked
                    // child) that is already in the process of exiting -- not a long-lived,
                    // truly-simultaneous conflict. Since pass 213's mmap-address-verification fix,
                    // a caller whose fixed request gets silently relocated now correctly fails the
                    // whole `mmap()`/`execve()` instead of continuing with corrupted address
                    // bookkeeping -- but that means this collision, previously merely "unsafely
                    // survived", now visibly BLOCKS ordinary concurrent execution of short-lived
                    // programs unless the transient case is given a chance to clear first. Bounded
                    // (5 attempts, 2ms apart -- 10ms worst case) and scoped to exactly this
                    // already-rare, already-slow path so it cannot meaningfully regress the common
                    // case; still holds `_fixed_addr_guard` throughout (only blocks OTHER threads'
                    // own `Replace`-mode fixed allocations, never `Hint`-mode/ordinary growth).
                    let mut collision = None;
                    for attempt in 0..5u32 {
                        let fc =
                            find_foreign_claim(suggested_range.clone(), current_claim_owner());
                        let stack_overlap = find_live_stack_overlap(suggested_range.clone());
                        litebox_util_log::debug!(
                            start:% = suggested_range.start, end:% = suggested_range.end,
                            attempt:% = attempt,
                            found:% = fc.is_some(), has_committed_page:% = has_committed_page,
                            self_owner:? = current_claim_owner(),
                            foreign_owner:? = fc.as_ref().map(|(_, owner)| *owner),
                            foreign_range:? = fc.as_ref().map(|(r, _)| (r.start, r.end)),
                            stack_overlap:? = stack_overlap.as_ref().map(|r| (r.start, r.end));
                            "allocate_pages: Replace-mode foreign-claim check"
                        );
                        if fc.is_none() && stack_overlap.is_none() {
                            collision = None;
                            break;
                        }
                        collision = Some(());
                        if attempt + 1 < 5 {
                            std::thread::sleep(core::time::Duration::from_millis(2));
                        }
                    }
                    collision.is_some()
                }
            {
                // A foreign claim used to be trusted outright here, on the reasoning that it
                // "is another still-live guest process's real memory ... never a stale leftover
                // safe to clobber". That is not true, and assuming it was is what made ld.so fail.
                //
                // Claims outlive their memory whenever a range is released by a path that does not
                // reach the release hook, so the registry can assert ownership over address space
                // Windows has already handed back. Ask Windows before believing it: if the range
                // holds no reserved or committed memory at all, the claim describes something that
                // no longer exists, and relocating away from it turns a perfectly good MAP_FIXED
                // into an `EEXIST` (`do_mmap` rightly refuses a fixed mapping that moved), which
                // surfaces as `libc.so.6: failed to map segment from shared object` and kills every
                // process that tries to start.
                //
                // A genuinely live collision -- two `ET_EXEC` binaries sharing a link-time base
                // while both alive via nested `vfork()`, the case the original comment describes --
                // still has real memory behind it and still relocates exactly as before.
                if range_holds_real_memory(&suggested_range) {
                    base_addr = core::ptr::null_mut();
                } else {
                    purge_stale_claims(&suggested_range);
                }
            } else {
                let diag_requested_start = suggested_range.start;
                process_memory_range_by_regions(
                    suggested_range,
                    |r, state| -> Result<bool, std::convert::Infallible> {
                        let ok = match state {
                            // In case the region is already reserved, we just need to commit it.
                            // In case the region is already committed, decommit and recommit it.
                            Win32_Memory::MEM_RESERVE | Win32_Memory::MEM_COMMIT => {
                                let mut was_mapped_view = false;
                                let mut view_mbi = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
                                let mut flank_before: Option<core::ops::Range<usize>> = None;
                                let mut flank_after: Option<core::ops::Range<usize>> = None;
                                // The destroyed view's TRUE extent, when it can be recovered.
                                // `None` means fall back to the old per-range behaviour.
                                let mut view_span: Option<core::ops::Range<usize>> = None;
                                if state == Win32_Memory::MEM_COMMIT {
                                    // TODO: handle this race condition properly.
                                    assert_eq!(
                                        fixed_address_behavior,
                                        FixedAddressBehavior::Replace,
                                        "raced with another memory allocator"
                                    );
                                    // Windows can reject a `VirtualFree(MEM_DECOMMIT)` spanning
                                    // `r` with ERROR_INVALID_PARAMETER if `r` straddles a
                                    // merged-region boundary between two distinct allocations, or
                                    // if `r` is actually a mapped view (needs `UnmapViewOfFileEx`
                                    // instead, see below). See docs/cow-mmap-fixed-address-design.md
                                    // ("Reclaiming an already-committed range") for the full
                                    // reasoning and live repros behind both cases.
                                    do_query_on_region(&mut view_mbi, r.start as *mut c_void);
                                    let mbi_type = view_mbi.Type;
                                    was_mapped_view = mbi_type == Win32_Memory::MEM_MAPPED
                                        || mbi_type == Win32_Memory::MEM_IMAGE;
                                    // A mapped view's `UnmapViewOfFileEx` (below) destroys the
                                    // WHOLE view, which can be wider than `r` (e.g. ld.so's one
                                    // whole-library CoW view vs. one PT_LOAD's MAP_FIXED sub-mmap).
                                    // The code below recovers the view's true extent and
                                    // re-commits the flanking remainder as zero-fill so a later
                                    // guest access faults safely instead of SIGSEGV-ing on freed
                                    // memory; see the doc section above for why this can't instead
                                    // restore the flanks' original CoW content, and its resulting
                                    // known limitation.
                                    let view_base = view_mbi.AllocationBase as usize;
                                    let mut view_limit = view_base;
                                    if was_mapped_view && view_base != 0 {
                                        loop {
                                            let mut m = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
                                            let queried = unsafe {
                                                Win32_Memory::VirtualQuery(
                                                    view_limit as *mut c_void,
                                                    &raw mut m,
                                                    core::mem::size_of::<
                                                        Win32_Memory::MEMORY_BASIC_INFORMATION,
                                                    >(),
                                                )
                                            } != 0;
                                            if !queried || m.AllocationBase as usize != view_base {
                                                break;
                                            }
                                            let next = m.BaseAddress as usize + m.RegionSize;
                                            // Defensive: never spin if Windows reports no forward
                                            // progress.
                                            if next <= view_limit {
                                                break;
                                            }
                                            view_limit = next;
                                        }
                                    }
                                    // Only trust the walk if it produced a span that actually
                                    // contains the caller's range; otherwise leave `view_span` as
                                    // `None` and let the recommit take the old path rather than act
                                    // on bounds that cannot be justified.
                                    if was_mapped_view
                                        && view_base != 0
                                        && view_base <= r.start
                                        && view_limit >= r.end
                                    {
                                        view_span = Some(view_base..view_limit);
                                        flank_before = if view_base < r.start {
                                            Some(view_base..r.start)
                                        } else {
                                            None
                                        };
                                        flank_after = if view_limit > r.end {
                                            Some(r.end..view_limit)
                                        } else {
                                            None
                                        };
                                    } else {
                                        let view_start = view_mbi.BaseAddress as usize;
                                        let view_end = view_start + view_mbi.RegionSize;
                                        flank_before = if was_mapped_view && view_start < r.start {
                                            Some(view_start..r.start)
                                        } else {
                                            None
                                        };
                                        flank_after = if was_mapped_view && view_end > r.end {
                                            Some(r.end..view_end)
                                        } else {
                                            None
                                        };
                                    }
                                    // allocate_pages reclaiming an already-committed range either
                                    // unmaps a live section view or decommits its pages. Both
                                    // destroy contents while leaving higher-level bookkeeping
                                    // intact, which is exactly the observed blackout: a DRM
                                    // scanout buffer that is never destroyed yet reads as
                                    // EXACTLY zero after a fork. Log the range so it can be
                                    // matched against the framebuffer's own mapping.
                                    if diag_mm_enabled() {
                                        litebox_util_log::debug!(
                                            start:% = r.start,
                                            end:% = r.end,
                                            len:% = r.len(),
                                            was_mapped_view:? = was_mapped_view;
                                            "diag-reclaim: allocate_pages destroying committed range"
                                        );
                                    }
                                    let decommit_ok = if was_mapped_view {
                                        (unsafe {
                                            UnmapViewOfFileEx(
                                                Win32_Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                                                    Value: r.start as *mut c_void,
                                                },
                                                0,
                                            )
                                        }) != 0
                                    } else {
                                        fn decommit_bisecting(range: core::ops::Range<usize>) -> bool {
                                            if range.is_empty() {
                                                return true;
                                            }
                                            let ok = unsafe {
                                                VirtualFree(
                                                    range.start as *mut c_void,
                                                    range.len(),
                                                    Win32_Memory::MEM_DECOMMIT,
                                                )
                                            } != 0;
                                            if ok {
                                                return true;
                                            }
                                            // Only bisect on the specific "spans multiple allocation
                                            // objects" error; any other failure should surface as-is.
                                            if unsafe { GetLastError() } != windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER
                                            {
                                                return false;
                                            }
                                            // A single page can't be split further; nothing left to try.
                                            // Use the fixed 4 KiB Windows page granularity here (not
                                            // the outer `ALIGN` const, which nested `fn`s can't see) --
                                            // any multiple of the true page size is a valid split point.
                                            const PAGE: usize = 0x1000;
                                            if range.len() <= PAGE {
                                                return false;
                                            }
                                            let mid = range.start
                                                + ((range.len() / 2) / PAGE).max(1) * PAGE;
                                            decommit_bisecting(range.start..mid)
                                                && decommit_bisecting(mid..range.end)
                                        }
                                        decommit_bisecting(r.clone())
                                    };
                                    if !decommit_ok {
                                        let mut mbi =
                                            Win32_Memory::MEMORY_BASIC_INFORMATION::default();
                                        do_query_on_region(&mut mbi, r.start as *mut c_void);
                                        litebox_util_log::debug!(
                                            start:% = r.start, end:% = r.end,
                                            mbi_type:? = mbi.Type, mbi_state:? = mbi.State,
                                            mbi_protect:? = mbi.Protect;
                                            "allocate_pages: DIAG decommit failed, dumping region info"
                                        );
                                    }
                                    assert!(
                                        decommit_ok,
                                        "VirtualFree(DECOMMIT) failed: {}",
                                        unsafe { GetLastError() }
                                    );
                                }
                                // `UnmapViewOfFileEx` (the `was_mapped_view` branch above) drops
                                // the freed range straight to `MEM_FREE`, not `MEM_RESERVE` --
                                // unlike `VirtualFree(MEM_DECOMMIT)`, which leaves the allocation
                                // reserved. `VirtualAlloc2(MEM_COMMIT)` alone requires an existing
                                // reservation, so a former mapped view needs the same
                                // reserve-and-commit path as a genuinely free region.
                                let ptr = if was_mapped_view {
                                    // `UnmapViewOfFileEx` above dropped the WHOLE view to
                                    // `MEM_FREE` (a mapped view has no partial-unmap form), not to
                                    // `MEM_RESERVE` as `VirtualFree(MEM_DECOMMIT)` would. So every
                                    // byte of the former view -- the caller's own sub-range `r` AND
                                    // the flanking remainder either side of it -- must be reserved
                                    // again before anything can be committed into it.
                                    //
                                    // Reserve the ENTIRE former view in ONE call, then `MEM_COMMIT`
                                    // each piece inside that reservation. Reserving each flank at
                                    // its OWN base (what this code used to do) fails with
                                    // `ERROR_INVALID_ADDRESS` (487) essentially every time:
                                    // `MEM_RESERVE` demands an allocation-granularity-aligned
                                    // (64 KiB) base and a flank boundary is only page-aligned --
                                    // it is wherever `r` happens to begin or end. Observed live,
                                    // three flanks in one run, every one page-aligned and none
                                    // granularity-aligned (0xA57000, 0xAB1000, 0xB6B000), each left
                                    // `MEM_FREE`. That is what made CPython SIGSEGV, with no
                                    // traceback, while loading selkies' `pixelflux`/`pcmflux`
                                    // native extensions -- and with them the whole webtop video
                                    // path.
                                    //
                                    // The reservation is rounded UP to granularity at its end. That
                                    // detail is load-bearing: leaving it at the view's exact end
                                    // strands the remainder of the final granule as free-but-
                                    // unreservable, so the next `mmap` landing there cannot reserve
                                    // it (our reservation already occupies the granule) and cannot
                                    // commit into it either (that tail was never reserved) -- an
                                    // immediate panic instead of the old latent SIGSEGV. Taking the
                                    // whole granule is safe: it was all part of this same view's
                                    // own reservation, which `UnmapViewOfFileEx` just released.
                                    // `reserve_and_commit` is what then commits into it, via its
                                    // already-reserved fallback.
                                    //
                                    // A view's `Protect` may be `PAGE_WRITECOPY` /
                                    // `PAGE_EXECUTE_WRITECOPY`, meaningful only for a mapped section
                                    // and rejected outright for PRIVATE anonymous memory. Map
                                    // copy-on-write down to its plain read/write equivalent: the
                                    // flank is being re-created as ordinary anonymous memory, so
                                    // there is no section left for copy-on-write to refer to.
                                    // (`MEM_FREE` reports `Protect == 0`, not a legal commit
                                    // protection either.)
                                    let anon_prot = match view_mbi.Protect {
                                        Win32_Memory::PAGE_WRITECOPY => Win32_Memory::PAGE_READWRITE,
                                        Win32_Memory::PAGE_EXECUTE_WRITECOPY => {
                                            Win32_Memory::PAGE_EXECUTE_READWRITE
                                        }
                                        0 => Win32_Memory::PAGE_READWRITE,
                                        other => other,
                                    };
                                    let reserved_whole = view_span.clone().is_some_and(|span| {
                                        let lo = span.start;
                                        let hi = self.round_up_to_granu(span.end);
                                        let got = unsafe {
                                            VirtualAlloc2(
                                                GetCurrentProcess(),
                                                lo as *mut c_void,
                                                hi - lo,
                                                Win32_Memory::MEM_RESERVE,
                                                Win32_Memory::PAGE_NOACCESS,
                                                core::ptr::null_mut(),
                                                0,
                                            )
                                        };
                                        if got.is_null() {
                                            litebox_util_log::error!(
                                                start:% = lo, end:% = hi,
                                                win32_err:% = unsafe { GetLastError() };
                                                "diag-reclaim: whole-view MEM_RESERVE failed, falling back to per-range reserve"
                                            );
                                            false
                                        } else {
                                            true
                                        }
                                    });
                                    if reserved_whole {
                                        // Inside our own reservation now, so each piece is a plain
                                        // `MEM_COMMIT` at its exact page-aligned bounds -- no
                                        // granularity constraint applies to a commit.
                                        //
                                        // The flanks cannot be restored as equivalent CoW mappings
                                        // (same file/offset): nothing here tracks which file backed
                                        // a given guest address once the view is gone (`VmArea`
                                        // records only `is_file_backed: bool`). They are therefore
                                        // re-created as anonymous zero-fill pages, which loses the
                                        // flanks' original file content -- a real, documented
                                        // limitation -- but converts a guaranteed SIGSEGV into a
                                        // zero-filled read, which is always memory-safe.
                                        for flank in
                                            [&flank_before, &flank_after].into_iter().flatten()
                                        {
                                            let flank_ptr = unsafe {
                                                VirtualAlloc2(
                                                    GetCurrentProcess(),
                                                    flank.start as *mut c_void,
                                                    flank.len(),
                                                    Win32_Memory::MEM_COMMIT,
                                                    anon_prot,
                                                    core::ptr::null_mut(),
                                                    0,
                                                )
                                            };
                                            if flank_ptr.is_null() {
                                                litebox_util_log::error!(
                                                    start:% = flank.start, end:% = flank.end,
                                                    win32_err:% = unsafe { GetLastError() };
                                                    "diag-reclaim: failed to commit orphaned CoW-view flank as anonymous memory -- next touch will SIGSEGV"
                                                );
                                            } else if diag_mm_enabled() {
                                                litebox_util_log::debug!(
                                                    start:% = flank.start, end:% = flank.end,
                                                    len:% = flank.len();
                                                    "diag-reclaim: committed orphaned CoW-view flank as anonymous zero-fill memory"
                                                );
                                            }
                                        }
                                        unsafe {
                                            VirtualAlloc2(
                                                GetCurrentProcess(),
                                                r.start as *mut c_void,
                                                r.len(),
                                                Win32_Memory::MEM_COMMIT,
                                                prot_flags(initial_permissions),
                                                core::ptr::null_mut(),
                                                0,
                                            )
                                        }
                                    } else {
                                        // Either the view's true extent could not be recovered or
                                        // its reservation could not be taken. Fall back to the
                                        // previous behaviour: reserve-and-commit just `r` and leave
                                        // the flanks unbacked. Strictly worse, but it is what this
                                        // code did before and it keeps the allocation succeeding.
                                        reserve_and_commit(
                                            r.clone(),
                                            prot_flags(initial_permissions),
                                            0,
                                        )
                                    }
                                } else {
                                    if diag_mm_enabled() {
                                        litebox_util_log::debug!(
                                            start:% = r.start, end:% = r.end, len:% = r.len(),
                                            pid:% = std::process::id(),
                                            tid:? = std::thread::current().id();
                                            "diag-commit: VirtualAlloc2(MEM_COMMIT) over reserved range"
                                        );
                                    }
                                    unsafe {
                                        VirtualAlloc2(
                                            GetCurrentProcess(),
                                            r.start as *mut c_void,
                                            r.len(),
                                            Win32_Memory::MEM_COMMIT,
                                            prot_flags(initial_permissions),
                                            core::ptr::null_mut(),
                                            0,
                                        )
                                    }
                                };
                                !ptr.is_null()
                            }
                            // In case the region is free, we need to reserve and commit it.
                            Win32_Memory::MEM_FREE => {
                                let ptr =
                                    reserve_and_commit(r.clone(), prot_flags(initial_permissions), 0);
                                !ptr.is_null()
                            }
                            _ => unimplemented!(
                                "Unexpected memory state: {:?} when allocating pages",
                                state
                            ),
                        };
                        // Prefetch the memory range if requested
                        if ok && populate_pages_immediately {
                            do_prefetch_on_range(r.start, r.len());
                        }
                        Ok(ok)
                    },
                )
                .unwrap();
                // Claimed for EVERY behavior, not just `Replace`.
                //
                // `claim_range`'s own doc comment already states that `Hint`-mode allocations
                // reach it -- but they only ever did via the OS-picks-the-address fallback at the
                // bottom of this function, which a hint reaches ONLY when it collides and gets
                // relocated. A hint that SUCCEEDS at the address it asked for landed here, where
                // the claim was gated on `Replace`, and was never recorded at all.
                //
                // That is not a rare corner: `Vmem::create_mapping` runs every ordinary guest
                // `mmap(NULL, ...)` through `get_unmmaped_area` FIRST, which always returns a
                // concrete non-zero address, and only then calls `insert_mapping` with `Hint`. So
                // `suggested_range.start != 0` holds for essentially every guest mmap, and the
                // uncollided majority of them were invisible to `find_foreign_claim` -- exactly
                // the memory a different thread's later `Replace`-mode fixed allocation (its own
                // `brk()` growth, or an `ET_EXEC` segment at a baked-in base) decommits and
                // recommits straight over, with no page fault and no guest-visible signal. It is
                // the same defect `claim_range`'s weston repro describes, left open for every
                // allocation that did not happen to be relocated first.
                //
                // `NoReplace` is claimed for the same reason: it commits real host memory too.
                claim_range(base_addr as usize..(base_addr as usize + size));
                // DIAG (AGENTS.md pass 223): allocation-free raw print of the actual returned
                // base_addr vs. the originally-requested suggested_range.start, specifically for
                // Replace-mode fixed calls -- to finally observe directly whether this success
                // path (reached whenever the collision/committed-page checks above do NOT
                // trigger) ever returns a MISMATCHED address, which pass 213's own downstream
                // check in litebox_common_linux::mm::do_mmap would then reject as EEXIST. Gated
                // on a mismatch only, so it cannot spam the log on the overwhelming common case
                // where this path already returns the correct address.
                if fixed_address_behavior == FixedAddressBehavior::Replace
                    && base_addr as usize != diag_requested_start
                {
                    diag_raw_print(
                        b"[diag-replace-mismatch] requested=0x",
                        diag_requested_start,
                        b" actual=0x",
                        base_addr as usize,
                    );
                }
                return Ok(UserMutPtr::from_ptr(base_addr.cast()));
            }
        }

        debug_assert!(base_addr.is_null());
        // Hold `ALLOCATE_PAGES_FIXED_ADDR_LOCK` here too, even though this specific
        // `VirtualAlloc2` call is itself atomic at the OS level (the reason the fixed-address
        // branch above's own doc comment gives for why this OS-picks-any-address path was
        // originally left unlocked). That reasoning is only half the story: `deallocate_pages`'s
        // own `VirtualQuery`-then-`VirtualFree` walk (see its doc comment) holds this SAME lock
        // specifically because Windows' VAD tree is a shared, mutable structure that a
        // concurrent `VirtualAlloc2` -- landing on the OS's own choice of address, not
        // necessarily far from whatever `deallocate_pages` is walking -- can still coalesce/split
        // nodes across. Leaving this path unlocked meant `deallocate_pages`'s own protection was
        // only ever one-sided: it serialized against a concurrent FIXED-address `allocate_pages`
        // call (which already took the lock) but never against this OS-picked-address path,
        // which every ordinary guest `mmap(NULL, ...)` uses -- confirmed live as the likely cause
        // of a real, reproducible `labwc` SIGSEGV under concurrent multi-process load (`labwc`,
        // `xfwm4`, `xfdesktop` all allocating/freeing via `mmap(NULL, ...)`/`munmap()`
        // concurrently): `labwc`'s own thread crashed immediately after its own successful
        // `munmap()` of a fresh `mmap(NULL, ...)` allocation, with a concurrent OTHER thread's
        // own heavy `mmap(NULL, ...)`/`munmap()` churn running at the exact same moment -- exactly
        // the shape this lock exists to prevent, just missing from this one path.
        let _fixed_addr_guard = ALLOCATE_PAGES_FIXED_ADDR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ptr = reserve_and_commit(0..size, prot_flags(initial_permissions), placement_floor);
        if ptr.is_null() && placement_floor != 0 {
            // The constrained retry could not be satisfied above `placement_floor` (genuinely
            // no room up there). Fall back to the original unconstrained behaviour rather than
            // turning a placement preference into an allocation failure -- a low address is
            // still correct, merely worse for locality.
            // Was unconditional `error!` here (any run taking this path silently regresses to
            // the old bottom-up packed layout, so it used to be logged unconditionally to make
            // that visible). Found, via a live WinDbg-confirmed fork-time crash, to itself be a
            // hazard: this call runs on the fork-relocation path inside `PageManager::duplicate`,
            // where a `tracing`-backed formatted log event (allocation + I/O) racing concurrent
            // Windows API activity on another thread has already been root-caused once before
            // (see `docs/track-b-fork-fix-progress.md`'s DIAG_HEAL entry) to crash the host with
            // an AV inside ntdll on a background thread. Gated behind `LITEBOX_DIAG_MM` like the
            // sibling diagnostic above, instead of removed outright, since this one is a real
            // regression signal worth keeping available on demand.
            if diag_mm_enabled() {
                litebox_util_log::error!(
                    floor:% = placement_floor, size:% = size;
                    "allocate_pages: constrained retry above discarded hint failed, retrying unconstrained"
                );
            }
            ptr = reserve_and_commit(0..size, prot_flags(initial_permissions), 0);
        }
        if ptr.is_null() {
            // Out of memory is a condition the GUEST asked for and the guest can be told about;
            // it is not a bug in this runtime, so it must not panic the host. `allocate_pages`
            // already returns `Result<_, AllocationError>`, `AllocationError::OutOfMemory`
            // already exists, and callers already handle it (`mm::allocator` retries against it,
            // and `mm::linux` maps it onward for the guest) -- the `assert!` that used to be here
            // simply bypassed all of that and took the whole process down instead of failing one
            // `mmap`.
            //
            // Observed live: a MATE desktop plus selkies encoding 1920x842 H.264 on a 16 GiB host
            // reached genuine Windows commit exhaustion, and a single 631 MiB request
            // (`VirtualAlloc2(RESERVE|COMMIT size=0x25a80000) failed: The paging file is too small
            // for this operation to complete. (os error 1455)`) killed the entire guest -- every
            // process in it, since they all share one address space -- at the moment video
            // capture had just started. Linux would have returned `ENOMEM` from that one `mmap`
            // and let the caller cope. This is the same defect class as `resize_mapping`'s
            // `unreachable!()` on an out-of-space expand, fixed earlier for the same reason.
            //
            // Deliberately logged, not silent: an allocation this large failing is worth seeing
            // even though it is now recoverable, and unlike the diagnostics above this path is by
            // definition rare, so the logging hazard those comments describe does not apply.
            litebox_util_log::error!(
                size:% = size,
                os_error:% = std::io::Error::last_os_error();
                "allocate_pages: VirtualAlloc2(RESERVE|COMMIT) failed, reporting OutOfMemory"
            );
            return Err(AllocationError::OutOfMemory);
        }

        // Prefetch the memory range if requested
        if populate_pages_immediately {
            do_prefetch_on_range(ptr as usize, size);
        }
        // Claim unconditionally here (unlike the fixed-address branch above, which only claims
        // for `Replace`): this is the OS-picks-any-address path, taken by every ordinary guest
        // `mmap(NULL, ...)` (`Hint` mode) in addition to the unconstrained `Replace`/`NoReplace`
        // case where `suggested_range.start == 0`. See `claim_range`'s doc comment for why a
        // `Hint`-mode commit needs to be visible to a later, different thread's `Replace`-mode
        // collision check.
        litebox_util_log::debug!(
            start:% = ptr as usize, end:% = (ptr as usize + size);
            "allocate_pages: claiming fresh-address (start==0 path) range"
        );
        claim_range(ptr as usize..(ptr as usize + size));
        Ok(UserMutPtr::from_ptr(ptr.cast::<u8>()))
    }

    fn release_mapping_claim(&self, range: core::ops::Range<usize>) {
        // See `unclaim_range` for why a claim outliving its memory is fatal rather than untidy.
        unclaim_range(range);
    }

    unsafe fn deallocate_pages(
        &self,
        range: core::ops::Range<usize>,
    ) -> Result<(), litebox::platform::page_mgmt::DeallocationError> {
        debug_assert_alignment!(range, ALIGN);
        // NOTE: the claim release does NOT live here. `deallocate_pages` is not reached for every
        // guest unmap (a subrange overlapping a shared view deliberately skips it, since a view
        // can only be unmapped whole), so releasing here left exactly those ranges claimed
        // forever. It now hangs off `release_mapping_claim`, which `Vmem::remove_mapping` calls
        // unconditionally.
        // Hold `ALLOCATE_PAGES_FIXED_ADDR_LOCK` across this entire query-then-decommit walk, for
        // the same reason `allocate_pages`'s fixed-address path holds it (see that call site's own
        // doc comment): `process_memory_range_by_regions`'s `VirtualQuery`-then-act loop has no
        // atomicity guarantee against a concurrently-running guest thread's own `VirtualFree`/
        // `VirtualAlloc2` call on the same or an adjacent region -- Windows' own VAD tree can
        // coalesce/split nodes spanning a query boundary, so an unlocked reader here could act on
        // a state that's already stale by the time it does. `deallocate_pages` previously took no
        // lock at all here, an asymmetry with `allocate_pages`'s own already-locked fixed-address
        // path -- closing it so both allocate and deallocate are serialized against each other.
        let _fixed_addr_guard = ALLOCATE_PAGES_FIXED_ADDR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        process_memory_range_by_regions(
            range.clone(),
            |r, state| -> Result<bool, std::convert::Infallible> {
                debug_assert_ne!(
                    state,
                    Win32_Memory::MEM_FREE,
                    "Trying to deallocate a free region: {:p}-{:p}",
                    r.start as *mut c_void,
                    r.end as *mut c_void
                );
                // `VirtualFree(MEM_DECOMMIT)` is only valid on privately-committed memory --
                // calling it on a mapped SECTION VIEW (`MEM_MAPPED`) is invalid on Windows and,
                // if litebox's own VMA bookkeeping ever fails to recognize a range as shared
                // (the caller only reaches `deallocate_pages` when it believes NO shared VMA
                // overlaps `range` -- see `Vmem::remove_mapping`'s `shared_overlaps.is_empty()`
                // gate), silently decommits real backing a live view still needs. Confirmed live:
                // a guest process's own ELF-loader trampoline mmap+munmap-trim sequence hit
                // exactly this gap, decommitting a small guard page inside a larger `VM_SHARED`
                // arena the guest was still actively using, producing a genuine SIGSEGV
                // (write-fault into now-decommitted memory) moments later. Query the region's
                // type before touching it and refuse to decommit a mapped view -- converting this
                // silent corruption into a loud, attributable error instead.
                let mut mbi = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
                let queried = unsafe {
                    Win32_Memory::VirtualQuery(
                        r.start as *const c_void,
                        &raw mut mbi,
                        core::mem::size_of::<Win32_Memory::MEMORY_BASIC_INFORMATION>(),
                    ) != 0
                };
                if queried && mbi.Type == Win32_Memory::MEM_MAPPED {
                    litebox_util_log::error!(
                        start:% = r.start,
                        end:% = r.end;
                        "diag-deallocate-refused: refusing to VirtualFree(MEM_DECOMMIT) a MEM_MAPPED section view -- litebox's VMA bookkeeping believes this range is unshared, but the real Windows allocation is a mapped view. Leaving it alone rather than corrupting a live shared mapping."
                    );
                    return Ok(true);
                }
                if diag_mm_enabled() {
                    litebox_util_log::debug!(
                        start:% = r.start, end:% = r.end, len:% = r.len(),
                        pid:% = std::process::id(), tid:? = std::thread::current().id();
                        "diag-decommit: VirtualFree(MEM_DECOMMIT)"
                    );
                }
                Ok(unsafe {
                    VirtualFree(r.start as *mut c_void, r.len(), Win32_Memory::MEM_DECOMMIT)
                } != 0)
            },
        )
        .expect("deallocate_pages failed");
        // Claim release does NOT happen here -- see the NOTE a few lines up: it is
        // `release_mapping_claim`'s job, which `Vmem::remove_mapping` already calls
        // unconditionally for every guest unmap via `unclaim_range`.
        //
        // This used to ALSO call `release_claim_range_for_current_thread` here, a second,
        // redundant release directly contradicting the NOTE above. That function matched
        // `CLAIMED_RANGES`' doc-commented coalescing behaviour (`claim_range` merges a guest
        // process's own touching/overlapping claims into one slot) with a release that, on any
        // PARTIAL overlap, dropped the ENTIRE merged slot rather than shrinking it -- unlike
        // `unclaim_range`, which correctly shrinks from whichever edge the freed sub-range
        // touches. Root-caused live: `elf_load`'s per-segment trampoline-extension padding
        // (a few KiB, freed right after use) routinely coalesces into the SAME `CLAIMED_RANGES`
        // slot as the real library image next to it (they are placed touching, and `claim_range`
        // treats "touching" as coalesce-eligible) -- freeing just the padding then deleted the
        // WHOLE slot, silently erasing collision-detection coverage for the entire
        // still-live library image beside it. A second, unrelated guest process that later
        // happened to request an overlapping address (confirmed live: a short-lived `sleep`
        // helper's own libc.so.6 load) then passed `find_foreign_claim` with no record left to
        // find, silently decommitted-and-recommitted straight over the first process's real,
        // already-populated library memory, and that process's own later read of it (~2.6s on
        // the observed Xvfb/libselinux.so.1 repro) faulted as genuinely not-present. Removing
        // this call (rather than fixing it to shrink like `unclaim_range`) is correct, not just
        // simpler: `release_mapping_claim` already runs unconditionally and already does this
        // right, so this call was always pure redundancy with a worse failure mode bolted on.
        Ok(())
    }

    unsafe fn update_permissions(
        &self,
        range: core::ops::Range<usize>,
        new_permissions: MemoryRegionPermissions,
    ) -> Result<(), litebox::platform::page_mgmt::PermissionUpdateError> {
        debug_assert_alignment!(range, ALIGN);
        let flags = prot_flags(new_permissions);
        // Hold `VIRTUAL_PROTECT_LOCK` for the whole region walk: see its doc comment for why an
        // unsynchronized `VirtualProtect` here can race `fork_verify`'s own temporary
        // protection-flip-and-restore on a page shared with an unrelated thread.
        let _guard = VIRTUAL_PROTECT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        process_memory_range_by_regions(
            range,
            |r, state| -> Result<bool, std::convert::Infallible> {
                debug_assert_eq!(
                    state,
                    Win32_Memory::MEM_COMMIT,
                    "Trying to change permissions on a non-committed region: {:p}-{:p}",
                    r.start as *mut c_void,
                    r.end as *mut c_void
                );
                let mut old_protect: u32 = 0;
                let ok = unsafe {
                    VirtualProtect(r.start as *mut c_void, r.len(), flags, &raw mut old_protect)
                } != 0;
                if diag_mm_enabled() {
                    litebox_util_log::debug!(
                        tid:? = std::thread::current().id(),
                        start:% = r.start,
                        end:% = r.end,
                        new_flags:% = flags,
                        old_protect:% = old_protect,
                        ok:% = ok;
                        "diag-vprotect: update_permissions VirtualProtect"
                    );
                }
                Ok(ok)
            },
        )
        .expect("update_permissions failed");
        Ok(())
    }

    fn reserved_pages(&self) -> impl Iterator<Item = &std::ops::Range<usize>> {
        self.reserved_pages.iter()
    }

    /// Windows analogue of `litebox_platform_linux_userland::LinuxUserland::try_allocate_cow_pages`
    /// (`mmap(MAP_PRIVATE, fd, offset)`'s direct equivalent): opens the real backing file
    /// (looked up via [`Self::lookup_cow_region`], populated by [`Self::register_cow_region`] at
    /// runner startup for the host-mmapped rootfs tar) with `CreateFileW`, creates a
    /// `PAGE_WRITECOPY`/`PAGE_EXECUTE_WRITECOPY` file mapping over it with `CreateFileMappingW`,
    /// and maps the requested sub-range with `MapViewOfFile3` -- giving the exact copy-on-write
    /// semantics `mmap(MAP_PRIVATE)` does: pages are shared read-only against the file's own page
    /// cache until a write occurs, at which point Windows privately copies just that one page.
    /// Both handles are closed immediately after the view is created (mirroring the Linux impl's
    /// `close(fd)` right after `mmap`) -- the OS keeps the mapping alive via the view itself, not
    /// the handles.
    ///
    /// Reuses `map_shared_memory`'s own `TASK_ADDR_MIN..TASK_ADDR_MAX`-bounded
    /// `MEM_ADDRESS_REQUIREMENTS`/`MEM_EXTENDED_PARAMETER` placement pattern (see that function's
    /// doc comment for the real host-allocator-aliasing bug this constraint exists to prevent --
    /// an unconstrained view placement is exactly as dangerous here) and the same
    /// `FixedAddressBehavior::Hint` null-address retry / `NoReplace` alignment-error-as-collision
    /// handling.
    ///
    /// # A real, permanent platform difference from the Linux impl
    ///
    /// `MapViewOfFile3` requires the VIEW's FILE OFFSET (not just its start address, which
    /// `map_shared_memory` already handles) to be a multiple of the system allocation granularity
    /// (64 KiB on every real Windows install), not merely page-aligned (4 KiB) the way Linux's
    /// `mmap` offset requirement is. ELF `PT_LOAD` segment file offsets are typically only
    /// 4 KiB-aligned, so this legitimately fails for many real segments -- there is no workaround
    /// (unlike `map_shared_memory`'s address-collision retry, this is a hard API requirement on
    /// the FILE offset itself, not a placement choice this code makes). On that failure this
    /// returns `CowAllocationError::Unaligned`, and the caller (`litebox_shim_linux`'s
    /// `try_cow_mmap_file`) falls back to the existing page-by-page memcpy path -- correct,
    /// expected behavior, not a bug: this fast path is a partial-coverage optimization (fast for
    /// however many real segments/mappings DO land on a 64 KiB-aligned file offset), not a
    /// universal replacement for the memcpy fallback.
    fn try_allocate_cow_pages(
        &self,
        suggested_start: usize,
        source_data: &'static [u8],
        permissions: MemoryRegionPermissions,
        fixed_address_behavior: FixedAddressBehavior,
        verified_safe_padding: usize,
    ) -> Result<(Self::RawMutPointer<u8>, Option<(usize, usize)>), CowAllocationError> {
        const ALLOCATION_GRANULARITY: usize = 0x1_0000;

        let Some((file_path, file_offset)) = self.lookup_cow_region(source_data) else {
            return Err(CowAllocationError::UnsupportedSourceRegion);
        };
        // `MapViewOfFile3` requires its `Offset` parameter to be a multiple of the allocation
        // granularity (64KiB), stricter than Linux `mmap`'s 4KiB page-offset requirement -- and
        // real ELF `PT_LOAD` segment file-offsets are only ever page-aligned (linker-controlled,
        // not something a packer can fix, see AGENTS.md pass 321/342's sizing data). The fix:
        // map the containing 64KiB-aligned file region instead and place the view's HOST base
        // `view_padding` bytes before `suggested_start`, so the returned content pointer still
        // equals `suggested_start` exactly (satisfying `Replace`/`NoReplace`'s exact-address
        // contract, see `try_cow_mmap_file` in `litebox_shim_linux/src/syscalls/mm.rs`).
        //
        // AGENTS.md pass 344 history: an EARLIER version of this exact trick (`Hint` case only)
        // shipped, then was found and reverted as a real, live memory-safety bug -- the
        // `view_padding` bytes before the returned pointer are real, physically-mapped,
        // host-present memory that `try_allocate_cow_pages` never reported to its caller, so
        // `Vmem` never learned about them: a guest touching that range faulted as "genuinely
        // unmapped" (no `Vmem` record) even though the OS had it mapped, and this codebase's
        // crash-cleanup path then called `VirtualFree(MEM_DECOMMIT)` on it, which fails outright
        // because the memory is a `MapViewOfFile3` view, not a `VirtualAlloc` region (only
        // `UnmapViewOfFileEx` is valid there). Reproduced deterministically at the time via
        // `/usr/bin/labwc --help` (`view_padding=61440`, fault at `cr2=0xa0f2a0`, `os error 487`).
        //
        // THIS FUNCTION NEVER MAKES THAT MISTAKE AGAIN BY CONSTRUCTION: it has NO `Vmem` access
        // at all (it lives in this platform crate; `Vmem` lives in `litebox_shim_linux`/`litebox`
        // core) and therefore cannot itself decide "this padding range is safe to host-map." The
        // `verified_safe_padding` parameter is the CALLER's own live, runtime query result
        // against its OWN `Vmem` state (see `try_cow_mmap_file`'s `get_memory_permissions` check,
        // run immediately before calling this function) confirming the exact
        // `[suggested_start - padding, suggested_start)` range is CURRENTLY a single, contiguous,
        // `PROT_NONE` VMA belonging to this process's own ELF reservation -- not a static
        // inference about what `ElfFile::reserve` "should" have left there, a live fact checked
        // at the moment it matters. This function only ever uses padding UP TO that
        // caller-verified amount (never more, regardless of what the file offset's own alignment
        // would otherwise need) and, on success, reports the exact padding range it actually used
        // back to the caller via `Ok`'s tuple so the caller can register it with `Vmem` BEFORE the
        // guest can ever observe or fault on it -- see the `Ok` arm below and this trait method's
        // own doc comment in `litebox/src/platform/page_mgmt.rs` for the full contract.
        let aligned_offset = file_offset - (file_offset % ALLOCATION_GRANULARITY);
        let needed_padding = file_offset - aligned_offset;
        if needed_padding > verified_safe_padding {
            if diag_mm_enabled() {
                litebox_util_log::debug!(
                    file_offset:% = file_offset, len:% = source_data.len(),
                    needed_padding:% = needed_padding, verified_safe_padding:% = verified_safe_padding;
                    "diag-cow: file offset not 64KiB-aligned and caller did not verify enough safe padding, falling back to memcpy path"
                );
            }
            return Err(CowAllocationError::Unaligned);
        }
        let view_padding = needed_padding;

        let file_path_wide: alloc::vec::Vec<u16> = file_path
            .as_os_str()
            .encode_wide()
            .chain(core::iter::once(0))
            .collect();

        let file_handle = unsafe {
            CreateFileW(
                file_path_wide.as_ptr(),
                Win32_Foundation::GENERIC_READ,
                FILE_SHARE_READ,
                core::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                Win32_Foundation::HANDLE::default(),
            )
        };
        if file_handle == Win32_Foundation::INVALID_HANDLE_VALUE {
            return Err(CowAllocationError::InternalFailure);
        }

        let map_protect = if permissions.contains(MemoryRegionPermissions::EXEC) {
            Win32_Memory::PAGE_EXECUTE_WRITECOPY
        } else {
            Win32_Memory::PAGE_WRITECOPY
        };
        let mapping_handle = unsafe {
            CreateFileMappingW(
                file_handle,
                core::ptr::null(),
                map_protect,
                0,
                0,
                core::ptr::null(),
            )
        };
        unsafe {
            let _ = windows_sys::Win32::Foundation::CloseHandle(file_handle);
        }
        if mapping_handle.is_null() {
            return Err(CowAllocationError::InternalFailure);
        }

        let mut addr_req = MEM_ADDRESS_REQUIREMENTS {
            LowestStartingAddress: <WindowsUserland as litebox::platform::PageManagementProvider<
                ALIGN,
            >>::TASK_ADDR_MIN as *mut c_void,
            HighestEndingAddress: (<WindowsUserland as litebox::platform::PageManagementProvider<
                ALIGN,
            >>::TASK_ADDR_MAX
                - 1) as *mut c_void,
            Alignment: 0,
        };
        let mut ext_param = MEM_EXTENDED_PARAMETER {
            Anonymous1: MEM_EXTENDED_PARAMETER_0 {
                _bitfield: MemExtendedParameterAddressRequirements as u64,
            },
            Anonymous2: windows_sys::Win32::System::Memory::MEM_EXTENDED_PARAMETER_1 {
                Pointer: (&raw mut addr_req).cast::<c_void>(),
            },
        };
        let view_protect = prot_flags(permissions);
        let map_offset = aligned_offset as u64;
        // The view must cover the padding prefix too when one is in use (`map_len` grows by
        // `view_padding`, always 0 unless `needed_padding <= verified_safe_padding` above), so
        // its OS-level `Offset` parameter can stay 64KiB-aligned while its CONTENT still starts
        // exactly at `source_data`'s own first byte, `view_padding` bytes into the view.
        let map_len = source_data.len() + view_padding;
        let mut try_map = |base_addr: *const c_void, constrained: bool| unsafe {
            if constrained {
                MapViewOfFile3(
                    mapping_handle,
                    GetCurrentProcess(),
                    base_addr,
                    map_offset,
                    map_len,
                    0,
                    view_protect,
                    &raw mut ext_param,
                    1,
                )
            } else {
                MapViewOfFile3(
                    mapping_handle,
                    GetCurrentProcess(),
                    base_addr,
                    map_offset,
                    map_len,
                    0,
                    view_protect,
                    core::ptr::null_mut(),
                    0,
                )
            }
        };
        // The VIEW's own base must sit `view_padding` bytes before `suggested_start` (so the
        // padding fills the low end of the view and `suggested_start` lands exactly
        // `view_padding` bytes into it) -- this is only meaningful/safe when `view_padding != 0`
        // was itself derived from `verified_safe_padding`, the caller's own live-checked-safe
        // range immediately preceding `suggested_start` (see the doc comment above).
        let base_addr = if suggested_start == 0 {
            core::ptr::null()
        } else {
            (suggested_start - view_padding) as *const c_void
        };
        let mut view = try_map(base_addr, base_addr.is_null());
        if view.Value.is_null()
            && !base_addr.is_null()
            && fixed_address_behavior == FixedAddressBehavior::Hint
        {
            view = try_map(core::ptr::null(), true);
        }
        unsafe {
            let _ = windows_sys::Win32::Foundation::CloseHandle(mapping_handle);
        }
        if view.Value.is_null() {
            let err = unsafe { GetLastError() };
            if diag_mm_enabled() {
                litebox_util_log::error!(
                    file_offset:% = file_offset, len:% = source_data.len(), win32_err:% = err;
                    "diag-cow: MapViewOfFile3 failed"
                );
            }
            if fixed_address_behavior == FixedAddressBehavior::NoReplace
                && (err == Win32_Foundation::ERROR_INVALID_ADDRESS
                    || err == Win32_Foundation::ERROR_MAPPED_ALIGNMENT)
            {
                return Err(CowAllocationError::InternalFailure);
            }
            return Err(CowAllocationError::InternalFailure);
        }
        // SAFETY: `view` covers exactly `map_len = source_data.len() + view_padding` bytes
        // starting at file offset `aligned_offset`; `view_padding` is exactly
        // `file_offset - aligned_offset`, so `view.Value + view_padding` is the address of file
        // offset `file_offset` -- the same content `source_data` itself starts at.
        let content_ptr = unsafe { view.Value.cast::<u8>().add(view_padding) };
        // Report the padding range this call ACTUALLY used back to the caller, computed from
        // where the view ACTUALLY landed (`view.Value`), never assumed from `suggested_start` --
        // the `Hint` unconstrained-retry path above can place the view anywhere Windows chooses,
        // so `content_ptr` does not necessarily equal `suggested_start` in that case, and the
        // padding range must be reported relative to the REAL returned pointer for the caller's
        // `Vmem` registration to be correct. `view_padding == 0` makes this a no-op range that
        // the caller correctly skips registering (see `try_cow_mmap_file`'s own handling).
        let padding_range = if view_padding == 0 {
            None
        } else {
            Some((view.Value as usize, view_padding))
        };
        if diag_mm_enabled() {
            litebox_util_log::debug!(
                addr:% = content_ptr as usize, len:% = source_data.len(), file_offset:% = file_offset,
                view_padding:% = view_padding;
                "diag-cow: try_allocate_cow_pages OK"
            );
        }
        Ok((UserMutPtr::from_ptr(content_ptr), padding_range))
    }

    // A Windows file-mapping `HANDLE`, backed by the system paging file (no real file on disk)
    // since litebox only uses this for anonymous `MAP_SHARED` memory. Cast to/from `usize` at
    // the trait boundary since `HANDLE` (a `*mut c_void`-shaped type) is not `Send`/`Sync` by
    // itself, but the raw value it wraps is just an opaque per-process kernel-object identifier
    // that is safe to copy and pass across threads (the same handle value is valid from any
    // thread of this process, per the Win32 handle model).
    type SharedMemoryHandle = usize;

    fn create_shared_memory(
        &self,
        size: usize,
    ) -> Result<Self::SharedMemoryHandle, SharedMemoryError> {
        let size_u64 = size as u64;
        // Intentional truncation: `CreateFileMappingW` takes the 64-bit size split into
        // high/low 32-bit halves, not a single 64-bit parameter.
        #[expect(clippy::cast_possible_truncation)]
        let handle = unsafe {
            CreateFileMappingW(
                Win32_Foundation::INVALID_HANDLE_VALUE,
                core::ptr::null(),
                Win32_Memory::PAGE_EXECUTE_READWRITE,
                (size_u64 >> 32) as u32,
                size_u64 as u32,
                core::ptr::null(),
            )
        };
        if handle.is_null() {
            return Err(SharedMemoryError::OutOfMemory);
        }
        if diag_mm_enabled() {
            litebox_util_log::debug!(
                handle:% = handle as usize, size:% = size, pid:% = std::process::id();
                "diag-shm: create_shared_memory"
            );
        }
        Ok(handle as usize)
    }

    fn create_named_shared_memory(
        &self,
        name: &str,
        size: usize,
    ) -> Result<Self::SharedMemoryHandle, SharedMemoryError> {
        let size_u64 = size as u64;
        // `Local\` scopes the object to this login session, matching every other named
        // kernel object this codebase creates (`xproc_sync::CrossProcessEvent::open`'s own doc
        // comment) -- every litebox process here is a descendant of the same runner in the same
        // session, so a session-scoped name is sufficient and avoids the `SeCreateGlobalPrivilege`
        // requirement `Global\` would add for no benefit.
        let wide: std::vec::Vec<u16> = name.encode_utf16().chain(core::iter::once(0)).collect();
        // Intentional truncation: `CreateFileMappingW` takes the 64-bit size split into
        // high/low 32-bit halves, not a single 64-bit parameter.
        #[expect(clippy::cast_possible_truncation)]
        let handle = unsafe {
            CreateFileMappingW(
                Win32_Foundation::INVALID_HANDLE_VALUE,
                core::ptr::null(),
                Win32_Memory::PAGE_EXECUTE_READWRITE,
                (size_u64 >> 32) as u32,
                size_u64 as u32,
                wide.as_ptr(),
            )
        };
        if handle.is_null() {
            let err = unsafe { GetLastError() };
            litebox_util_log::error!(
                name:% = name, size:% = size, win32_err:% = err;
                "diag-shm: create_named_shared_memory FAILED"
            );
            return Err(SharedMemoryError::OutOfMemory);
        }
        // `ERROR_ALREADY_EXISTS` here is the normal, expected outcome for every caller after the
        // first (see `CrossProcessEvent::open`'s own doc comment for the identical reasoning) --
        // `handle` refers to the pre-existing object in that case, sized as the FIRST caller
        // requested, exactly as this method's own doc comment states.
        if diag_mm_enabled() {
            let existed = unsafe { GetLastError() } == Win32_Foundation::ERROR_ALREADY_EXISTS;
            litebox_util_log::debug!(
                handle:% = handle as usize, name:% = name, size:% = size, existed:% = existed,
                pid:% = std::process::id();
                "diag-shm: create_named_shared_memory"
            );
        }
        Ok(handle as usize)
    }

    fn map_shared_memory(
        &self,
        handle: Self::SharedMemoryHandle,
        suggested_range: core::ops::Range<usize>,
        initial_permissions: MemoryRegionPermissions,
        fixed_address_behavior: FixedAddressBehavior,
    ) -> Result<Self::RawMutPointer<u8>, SharedMemoryError> {
        debug_assert_alignment!(suggested_range, ALIGN);
        // Mirrors `allocate_pages`'s `reserve_and_commit` null-address branch (see
        // `HOST_ALLOCATOR_REGION_MIN`'s doc comment): when no hint address is available (either
        // `suggested_range.start == 0`, or a hinted address was rejected and we fall back to
        // asking Windows to place it), `MapViewOfFile3` used to be called with a null base
        // address and NO `MEM_EXTENDED_PARAMETER` array at all -- completely unconstrained,
        // unlike every other guest allocation path in this file. Windows was free to satisfy
        // that request from anywhere, including inside `HOST_ALLOCATOR_REGION_MIN..`, the host
        // global allocator's exclusive region, silently aliasing an anonymous `MAP_SHARED`
        // guest mapping with host allocator memory. Constrain the null-address case the same
        // way, via `MEM_ADDRESS_REQUIREMENTS`.
        let mut addr_req = MEM_ADDRESS_REQUIREMENTS {
            LowestStartingAddress: <WindowsUserland as litebox::platform::PageManagementProvider<
                ALIGN,
            >>::TASK_ADDR_MIN as *mut c_void,
            HighestEndingAddress: (<WindowsUserland as litebox::platform::PageManagementProvider<
                ALIGN,
            >>::TASK_ADDR_MAX
                - 1) as *mut c_void,
            Alignment: 0,
        };
        let mut ext_param = MEM_EXTENDED_PARAMETER {
            Anonymous1: MEM_EXTENDED_PARAMETER_0 {
                _bitfield: MemExtendedParameterAddressRequirements as u64,
            },
            Anonymous2: windows_sys::Win32::System::Memory::MEM_EXTENDED_PARAMETER_1 {
                Pointer: (&raw mut addr_req).cast::<c_void>(),
            },
        };
        let mut try_map = |base_addr: *const c_void, constrained: bool| unsafe {
            if constrained {
                MapViewOfFile3(
                    handle as *mut c_void,
                    GetCurrentProcess(),
                    base_addr,
                    0,
                    suggested_range.len(),
                    0,
                    prot_flags(initial_permissions),
                    &raw mut ext_param,
                    1,
                )
            } else {
                MapViewOfFile3(
                    handle as *mut c_void,
                    GetCurrentProcess(),
                    base_addr,
                    0,
                    suggested_range.len(),
                    0,
                    prot_flags(initial_permissions),
                    core::ptr::null_mut(),
                    0,
                )
            }
        };
        let base_addr = if suggested_range.start == 0 {
            core::ptr::null()
        } else {
            suggested_range.start as *const c_void
        };
        let mut view = try_map(base_addr, base_addr.is_null());
        // `Hint` means the platform may pick a different address if the hint isn't available
        // (matching `allocate_pages`'s handling of the same case): retry with no address hint
        // rather than surfacing an address collision as an error.
        if view.Value.is_null()
            && !base_addr.is_null()
            && fixed_address_behavior == FixedAddressBehavior::Hint
        {
            view = try_map(core::ptr::null(), true);
        }
        if view.Value.is_null() {
            let err = unsafe { GetLastError() };
            litebox_util_log::debug!(
                base_addr:% = base_addr as usize, len:% = suggested_range.len(),
                fixed_address_behavior:? = fixed_address_behavior, win32_err:% = err;
                "map_shared_memory: DIAG MapViewOfFile3 failed"
            );
            litebox_util_log::error!(
                handle:% = handle as usize, pid:% = std::process::id(), win32_err:% = err;
                "diag-shm: map_shared_memory FAILED"
            );
            if fixed_address_behavior == FixedAddressBehavior::NoReplace
                && (err == Win32_Foundation::ERROR_INVALID_ADDRESS
                    || err == Win32_Foundation::ERROR_MAPPED_ALIGNMENT)
            {
                // ERROR_MAPPED_ALIGNMENT (1132): MapViewOfFile3 requires any explicit
                // fixed-address request to be allocation-granularity aligned (64 KiB), not
                // just page aligned (4 KiB). A page-aligned-but-not-granularity-aligned
                // target (e.g. an in-place shared-mapping expand target computed by
                // `resize_mapping`) hits this, not ERROR_INVALID_ADDRESS. Treat it the same
                // way: report it as an ordinary address-in-use collision so the caller's
                // real retry/placement-search path (`move_mappings`) picks a fresh,
                // granularity-valid address instead of this surfacing as a permanent ENOMEM.
                return Err(SharedMemoryError::AddressInUse);
            }
            return Err(SharedMemoryError::OutOfMemory);
        }
        // Sample the mapped content. The scanout path is already instrumented; this
        // covers the CLIENT surface buffers, which is what decides whether weston
        // fails to composite a surface that HAS content (a compositing bug) or is
        // correctly compositing a surface whose content never landed (a shared-memory
        // bug on the client-buffer path). Only small buffers are sampled: the 8.29MB
        // scanout has its own dedicated diagnostics and scanning it here would slow
        // every flip.
        if diag_mm_enabled() {
            let sample_len = core::cmp::min(suggested_range.len(), 4096);
            let nz = if sample_len > 0 && suggested_range.len() <= 4 * 1024 * 1024 {
                let bytes = unsafe {
                    core::slice::from_raw_parts(view.Value.cast::<u8>(), sample_len)
                };
                bytes.iter().filter(|b| **b != 0).count()
            } else {
                usize::MAX // not sampled
            };
            litebox_util_log::debug!(
                handle:% = handle as usize,
                pid:% = std::process::id(),
                addr:% = view.Value as usize,
                size:% = suggested_range.len(),
                nonzero_in_sample:% = nz;
                "diag-shm: map_shared_memory OK"
            );

            // Same-instant cross-mapping comparison. Two guest processes each map a
            // shared pool at different addresses; sampling each only at ITS OWN map
            // time cannot show whether they see the same bytes, because the samples
            // are taken at different moments. Keeping every live mapping of a handle
            // lets us read them ALL right now, so a divergence means the client and
            // the compositor genuinely disagree about the buffer's contents -- which
            // would be a litebox shared-mapping bug rather than a compositing one.
            if nz != usize::MAX {
                let mut live = ADV_SHM_VIEWS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                live.entry(handle as usize)
                    .or_insert_with(Vec::new)
                    .push((view.Value as usize, suggested_range.len()));
                if let Some(views) = live.get(&(handle as usize))
                    && views.len() > 1
                {
                    let readings: Vec<(usize, usize)> = views
                        .iter()
                        .map(|(a, l)| {
                            let n = core::cmp::min(*l, 4096);
                            // SAFETY: every address here was returned by MapViewOfFile3
                            // for this handle and is not unmapped until the guest drops it.
                            let b = unsafe { core::slice::from_raw_parts(*a as *const u8, n) };
                            (*a, b.iter().filter(|x| **x != 0).count())
                        })
                        .collect();
                    let distinct: std::collections::BTreeSet<usize> =
                        readings.iter().map(|(_, n)| *n).collect();
                    litebox_util_log::debug!(
                        handle:% = handle as usize,
                        views:? = readings,
                        agree:? = distinct.len() == 1;
                        "diag-shm-crossview"
                    );
                }
            }
        }
        Ok(UserMutPtr::from_ptr(view.Value.cast::<u8>()))
    }

    unsafe fn unmap_shared_memory(
        &self,
        range: core::ops::Range<usize>,
    ) -> Result<(), SharedMemoryError> {
        debug_assert_alignment!(range, ALIGN);
        // Drop this view from the cross-view registry BEFORE unmapping it. Without
        // this, a later same-instant sample would read through an address that no
        // longer exists and take an access violation -- which is exactly what
        // happened on the first attempt at this diagnostic.
        {
            let mut live = ADV_SHM_VIEWS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for views in live.values_mut() {
                views.retain(|(addr, _)| *addr != range.start);
            }
        }
        // Hold `VIRTUAL_PROTECT_LOCK` across this unmap: without it, this `UnmapViewOfFileEx`
        // call raced `update_permissions`'s own locked `VirtualQuery`-then-`VirtualProtect`
        // sequence on the SAME shared section view -- e.g. a guest process's real munmap()/exit
        // teardown of a `VM_SHARED` mapping (see `litebox/src/mm/linux.rs`'s `unmap_shared_memory`
        // caller) racing a different guest thread's `mprotect()` on that same shared region.
        // `update_permissions` queries the region as `MEM_COMMIT` and then calls `VirtualProtect`
        // on it, but between those two steps this unlocked path could free the whole view out
        // from under it, leaving `VirtualProtect` to observe `MEM_FREE` and fail with a spurious
        // `ERROR_SUCCESS` last-error -- confirmed live as a real host-process panic
        // (`process_memory_range_by_regions`'s `assert!(success, ...)`) during an actual XFCE/
        // Xwayland launch, immediately following a crashing guest process's shared-memory
        // teardown. `VIRTUAL_PROTECT_LOCK` already unifies every other Windows VAD-tree mutator
        // in this file (`VirtualProtect`, `VirtualFree`, `VirtualAlloc2` fixed-address paths --
        // see that constant's own doc comment); this call was the one remaining VAD-tree mutator
        // outside that unification.
        let _guard = VIRTUAL_PROTECT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if diag_mm_enabled() {
            litebox_util_log::debug!(
                start:% = range.start, end:% = range.end, len:% = range.len(),
                pid:% = std::process::id(), tid:? = std::thread::current().id();
                "diag-decommit: UnmapViewOfFileEx"
            );
        }
        let ok = unsafe {
            UnmapViewOfFileEx(
                Win32_Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: range.start as *mut c_void,
                },
                0,
            )
        } != 0;
        if !ok {
            // AGENTS.md pass 255-256: this branch previously reported every failure as
            // `SharedMemoryError::Unaligned` regardless of the REAL Windows error, swallowing
            // the actual cause -- surfaced live as a confusing `UnmapError(Unaligned)` panic
            // during a real weston/XFCE launch with no way to tell whether the range genuinely
            // was misaligned or `UnmapViewOfFileEx` failed for some other reason (e.g. the view
            // already unmapped, a stale/reused address, or a real Windows error). Diagnostic
            // print (unconditional, this call is already on the rare failure path so it costs
            // nothing on the common success path) to capture the real `GetLastError()` code the
            // next time this fires.
            let win_err = unsafe { GetLastError() };
            diag_raw_print(
                b"[diag-unmap-shared-fail] range.start=0x",
                range.start,
                b" win_err=0x",
                win_err as usize,
            );
            // Map the REAL Windows error code to the closest `SharedMemoryError` variant instead
            // of collapsing every failure into a generic `Unaligned` -- a future diagnostic (this
            // fix's own motivating case: `remove_mapping` calling this with a stale, shrunken
            // base address) sees which platform-level failure actually happened, not just a
            // blanket "unaligned" label that was never true in the first place.
            // `ERROR_INVALID_ADDRESS` (0x1e7 / 487): the given `range.start` is not a view's real
            // original base -- closest existing variant is `AddressInUse` (this call's `range`
            // argument IS an address, and this is fundamentally an address-validity failure, not
            // an alignment one).
            const ERROR_INVALID_ADDRESS: u32 = 487;
            // `ERROR_NOT_ENOUGH_MEMORY`/`ERROR_OUTOFMEMORY`: genuine resource exhaustion.
            const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
            const ERROR_OUTOFMEMORY: u32 = 14;
            return Err(match win_err {
                ERROR_INVALID_ADDRESS => SharedMemoryError::AddressInUse,
                ERROR_NOT_ENOUGH_MEMORY | ERROR_OUTOFMEMORY => SharedMemoryError::OutOfMemory,
                _ => SharedMemoryError::Unaligned,
            });
        }
        Ok(())
    }

    fn close_shared_memory(
        &self,
        handle: Self::SharedMemoryHandle,
    ) -> Result<(), SharedMemoryError> {
        // Best-effort: a `CloseHandle` failure here would mean the handle was already invalid,
        // which is not actionable by the caller (the shared memory is either already gone or was
        // never valid) -- matching this file's existing style of not treating cleanup-path
        // failures as fatal (see e.g. `VirtualFree` callers that only assert in truly
        // unexpected cases).
        if diag_mm_enabled() {
            litebox_util_log::debug!(
                handle:% = handle, pid:% = std::process::id();
                "diag-shm: close_shared_memory"
            );
        }
        let _ = unsafe { Win32_Foundation::CloseHandle(handle as *mut c_void) };
        Ok(())
    }
}

/// Background state backing [`read_from_raw_handle`]/[`stdin_ready_raw_handle`] for a real console
/// (`FILE_TYPE_CHAR`) `STD_INPUT_HANDLE`.
///
/// # Why a background reader thread, not `PeekConsoleInputW`
///
/// An earlier version of this readiness probe used `GetNumberOfConsoleInputEvents`/
/// `PeekConsoleInputW` to inspect the console's raw `INPUT_RECORD` queue directly, on the
/// assumption that "a queued key-down record" and "`ReadFile` would return immediately" were the
/// same fact. They are not, once `ENABLE_LINE_INPUT` (the default console mode, and the mode a
/// real interactive shell like `ash` runs under) is in play: conhost's cooked-read line editor
/// consumes `KEY_EVENT_RECORD`s out of the raw queue as they arrive (to echo them and perform
/// line editing), and only stages the finished line -- terminated by Enter -- in its own private,
/// unqueryable buffer. `ReadFile`/`ReadConsole`, however small the requested byte count, drain
/// that private cooked-read buffer directly, not the raw `INPUT_RECORD` queue. Confirmed live via
/// the ConPTY test harness (see this fix's commit message) and independently corroborated by
/// Microsoft Terminal maintainers (`microsoft/terminal#12143`): once a full line has been typed
/// and Enter pressed, a single small `ReadFile` call correctly drains the first few bytes, but the
/// *remaining* buffered bytes of that already-committed line are invisible to
/// `PeekConsoleInputW`/`GetNumberOfConsoleInputEvents` (they undercount to 0 or a stale
/// non-key-down remnant), even though `ReadFile` would still return them immediately with no
/// blocking. There is no supported Win32 API to peek cooked-read readiness -- the community-
/// converged workaround (also used by libraries like `system-terminal`) is what this does: run a
/// background thread doing nothing but blocking `ReadFile` calls, and treat *that thread's*
/// buffered results, not the raw input queue, as the readiness signal.
struct ConsoleStdinReader {
    /// Bytes already read from the console but not yet consumed by a guest `read()` call.
    buffer: std::sync::Mutex<std::collections::VecDeque<u8>>,
    /// Signaled whenever `buffer` transitions from empty to non-empty, or `eof` becomes true.
    ready: std::sync::Condvar,
    eof: core::sync::atomic::AtomicBool,
}

impl ConsoleStdinReader {
    /// Returns `platform`'s lazily-initialized [`ConsoleStdinReader`], spawning its background
    /// reader thread the first time this is called for a given `WindowsUserland` instance.
    ///
    /// Takes `&'static WindowsUserland` (not just `&WindowsUserland`) because the spawned reader
    /// thread's closure must outlive the calling stack frame -- `WindowsUserland::new` always
    /// hands back a `&'static Self` in practice (there is exactly one platform instance per
    /// process, leaked for its lifetime), so every real caller already has one.
    fn get(platform: &'static WindowsUserland) -> &'static Self {
        platform.console_stdin_reader.get_or_init(|| {
            let reader = ConsoleStdinReader {
                buffer: std::sync::Mutex::new(std::collections::VecDeque::new()),
                ready: std::sync::Condvar::new(),
                eof: core::sync::atomic::AtomicBool::new(false),
            };
            // Same-sized as every other guest-work-capable thread (see the sibling
            // `litebox-console-resize-watcher` thread's own stack-size fix, above in this file, for
            // why): this thread's blocking `ReadFile` loop can be interrupted and its underlying OS
            // thread reused to service a real guest syscall (via `syscall_callback`) once the loop
            // returns/exits, and Rust's plain default stack is undersized for that -- observed live
            // (pass 170, this investigation) as the same class of bug manifesting in a DIFFERENT
            // lazily-spawned background thread (`sys_write`'s own prologue faulting on a plain
            // stack-relative store under heavy `fork_verify` load).
            const GUEST_THREAD_STACK_SIZE: usize = 32 * 1024 * 1024;
            std::thread::Builder::new()
                .name("litebox-console-stdin-reader".to_owned())
                .stack_size(GUEST_THREAD_STACK_SIZE)
                .spawn(move || Self::reader_thread_body(platform))
                .expect("failed to spawn console stdin reader thread");
            reader
        })
    }

    /// Chunk size for each background `ReadFile` call. Sized to comfortably hold a typical typed
    /// line; larger than this just means a following `ConsoleStdinReader::read` call drains it
    /// across more than one guest `read()` invocation, matching how a real Linux pipe/tty already
    /// behaves for an over-long line.
    const CHUNK_LEN: u32 = 4096;

    /// Clears `ENABLE_LINE_INPUT` on `STD_INPUT_HANDLE`, once, before the reader thread's first
    /// `ReadFile` call.
    ///
    /// # Why this is required for correctness, not just a latency optimization
    ///
    /// With the console's default `ENABLE_LINE_INPUT` ("cooked mode") active, `ReadFile` does not
    /// release ANY buffered bytes to the caller until a full line (terminated by Enter) is
    /// available -- confirmed via a minimal, litebox-free repro: writing `ESC[6n` (a cursor-
    /// position-report query, which busybox ash's line editor issues via `ask_terminal()` when
    /// drawing a prompt) causes conhost to genuinely inject the `ESC[row;colR` reply into the
    /// console's raw input queue near-instantly (visible via `PeekConsoleInputW`), but a
    /// concurrently-blocked `ReadFile` call does NOT return with those bytes -- it stays blocked
    /// indefinitely, because the reply has no trailing Enter and cooked-mode line buffering will
    /// not release a partial line. The reply only becomes readable once concatenated with
    /// whatever the user types *next*, which corrupts ash's own escape-sequence/line-buffer
    /// state (`libbb/read_key.c`'s CPR-scanning loop and lineedit.c's stateful `read_key_buffer`)
    /// -- observed live as `ls /` corrupted into `ls: /<3 garbage bytes>: Invalid argument`,
    /// reproducible on the *second* interactive command in a session (the first has no pending,
    /// still-unread CPR reply from an earlier prompt draw to collide with).
    ///
    /// Since the guest (`ash`) already performs its own line editing character-by-character via
    /// its own `read(2)` loop (confirmed via syscall tracing: every guest read requests exactly 1
    /// byte), there is no reason for the *Windows* console to also cook/line-buffer input on top
    /// -- doing so is actively harmful here, not merely redundant. Clearing `ENABLE_LINE_INPUT`
    /// makes `ReadFile` release each byte (or escape-sequence reply) as soon as it is queued,
    /// exactly matching a real Linux tty's raw-mode delivery semantics and closing this race.
    /// `ENABLE_ECHO_INPUT`/`ENABLE_PROCESSED_INPUT` are deliberately left untouched: local
    /// character echo and Ctrl+C/Ctrl+Z signal generation continue to work exactly as before --
    /// this is not a full raw-mode switch, only the minimum change needed to stop the console
    /// from withholding already-arrived bytes behind an unrelated future line terminator.
    ///
    /// Best-effort: `SetConsoleMode` can return a nonzero-`GetLastError` "failure" on some
    /// ConPTY-backed handles even though the mode change visibly takes effect (confirmed via the
    /// same repro: `GetConsoleMode` read back afterward reflects the change, and the CPR-reply
    /// race closes, despite `GetLastError() == ERROR_INVALID_PARAMETER`) -- so this does not
    /// panic or retry on failure, only attempts the change once.
    fn disable_line_input_mode() {
        use windows_sys::Win32::System::Console::{
            ENABLE_LINE_INPUT, GetConsoleMode, STD_INPUT_HANDLE, SetConsoleMode,
        };

        let handle = unsafe { windows_sys::Win32::System::Console::GetStdHandle(STD_INPUT_HANDLE) };
        if handle.is_null() || handle == Win32_Foundation::INVALID_HANDLE_VALUE {
            // No real console attached (e.g. redirected pipe/file stdin): nothing to do, and
            // `read_from_raw_handle`'s non-`FILE_TYPE_CHAR` path never routes through this reader
            // anyway.
            return;
        }
        let mut mode: u32 = 0;
        if unsafe { GetConsoleMode(handle, &raw mut mode) } == 0 {
            // Not actually a console handle (e.g. a redirected pipe reports `FILE_TYPE_CHAR` in
            // some edge cases) -- nothing to change.
            return;
        }
        let _ = unsafe { SetConsoleMode(handle, mode & !ENABLE_LINE_INPUT) };
    }

    /// Runs on a dedicated background thread for the lifetime of the process: repeatedly issues a
    /// single genuinely blocking `ReadFile` against `STD_INPUT_HANDLE` and appends whatever comes
    /// back to `buffer`, waking any waiter. This is the only thread that ever calls `ReadFile` on
    /// the console handle, so its blocking is invisible to every guest thread -- they only ever
    /// observe this struct's already-buffered results.
    fn reader_thread_body(platform: &'static WindowsUserland) {
        use windows_sys::Win32::Storage::FileSystem::ReadFile;
        use windows_sys::Win32::System::Console::STD_INPUT_HANDLE;

        let this = Self::get(platform);
        Self::disable_line_input_mode();
        loop {
            let handle =
                unsafe { windows_sys::Win32::System::Console::GetStdHandle(STD_INPUT_HANDLE) };
            if handle.is_null() || handle == Win32_Foundation::INVALID_HANDLE_VALUE {
                this.eof.store(true, Ordering::SeqCst);
                this.ready.notify_all();
                return;
            }
            let mut chunk = [0u8; Self::CHUNK_LEN as usize];
            let mut read: u32 = 0;
            let ok = unsafe {
                ReadFile(
                    handle,
                    chunk.as_mut_ptr(),
                    Self::CHUNK_LEN,
                    &raw mut read,
                    core::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let err = unsafe { GetLastError() };
                // `ERROR_BROKEN_PIPE` (the write end of a redirected pipe closed) is EOF, matching
                // a real Linux `read()` on a closed pipe's read end.
                if err == Win32_Foundation::ERROR_BROKEN_PIPE {
                    this.eof.store(true, Ordering::SeqCst);
                    this.ready.notify_all();
                    return;
                }
                panic!("ReadFile(STD_INPUT_HANDLE) failed: error={err}");
            }
            if read == 0 {
                // A successful zero-byte read is EOF (matches `read_from_raw_handle`'s previous
                // direct-call contract).
                this.eof.store(true, Ordering::SeqCst);
                this.ready.notify_all();
                return;
            }
            // `read <= CHUNK_LEN` (4096), which fits in `usize` on every supported target.
            let read = usize::try_from(read).unwrap_or(chunk.len());
            let mut buffer = this.buffer.lock().unwrap();
            let was_empty = buffer.is_empty();
            buffer.extend(&chunk[..read]);
            drop(buffer);
            if was_empty {
                this.ready.notify_all();
            }
        }
    }

    /// Copies already-buffered bytes into `buf` (up to `buf.len()`), blocking until at least one
    /// byte is available or EOF is reached. Never itself calls `ReadFile`.
    fn read(&self, buf: &mut [u8]) -> usize {
        let mut buffer = self.buffer.lock().unwrap();
        loop {
            if !buffer.is_empty() {
                let len = buffer.len().min(buf.len());
                for slot in &mut buf[..len] {
                    *slot = buffer.pop_front().unwrap();
                }
                return len;
            }
            if self.eof.load(Ordering::SeqCst) {
                return 0;
            }
            buffer = self.ready.wait(buffer).unwrap();
        }
    }

    /// Non-blocking: `true` if a [`Self::read`] call right now would return immediately (either
    /// real buffered bytes, or EOF).
    fn is_ready(&self) -> bool {
        !self.buffer.lock().unwrap().is_empty() || self.eof.load(Ordering::SeqCst)
    }
}

/// Reads directly from the process's raw `STD_INPUT_HANDLE`, bypassing `std::io::stdin()`.
///
/// See the doc comment on [`write_to_raw_handle`] for why this deliberately avoids the `std::io`
/// wrappers: the exact same cross-guest-"process" lock-starvation hazard applies symmetrically to
/// `std::io::Stdin`'s internal buffered-reader lock.
///
/// For a real console (`FILE_TYPE_CHAR`), this drains [`ConsoleStdinReader`]'s buffer (see its doc
/// comment for why a background reader thread is required, not a direct `ReadFile` here) rather
/// than calling `ReadFile` itself. For a pipe/regular file, `ReadFile` remains safe to call
/// directly (no cooked-read desync applies to non-console handles), so this still issues it inline
/// for those, preserving the original direct-call behavior and error handling.
#[expect(
    clippy::unnecessary_wraps,
    reason = "mirrors StdioProvider::read_from_stdin's Result signature (and write_to_raw_handle's shape) even though every current failure path here maps to Ok(0)/EOF rather than a real Err; keeps the two raw-handle helpers symmetric and leaves room for a genuine error case without a signature change"
)]
fn read_from_raw_handle(
    platform: &'static WindowsUserland,
    buf: &mut [u8],
) -> Result<usize, litebox::platform::StdioReadError> {
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_CHAR, GetFileType, ReadFile};
    use windows_sys::Win32::System::Console::STD_INPUT_HANDLE;

    let handle = unsafe { windows_sys::Win32::System::Console::GetStdHandle(STD_INPUT_HANDLE) };
    if handle.is_null() || handle == Win32_Foundation::INVALID_HANDLE_VALUE {
        // No console/redirected input attached at all: behave like an already-closed stdin.
        return Ok(0);
    }
    if unsafe { GetFileType(handle) } == FILE_TYPE_CHAR {
        return Ok(ConsoleStdinReader::get(platform).read(buf));
    }
    let mut read: u32 = 0;
    let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    let ok = unsafe {
        ReadFile(
            handle,
            buf.as_mut_ptr(),
            len,
            &raw mut read,
            core::ptr::null_mut(),
        )
    };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        // `ERROR_BROKEN_PIPE` (the write end of a redirected pipe closed) is EOF, matching a
        // real Linux `read()` on a closed pipe's read end -- not an error condition to panic on.
        if err == Win32_Foundation::ERROR_BROKEN_PIPE {
            return Ok(0);
        }
        panic!("ReadFile(STD_INPUT_HANDLE) failed: error={err}");
    }
    Ok(read as usize)
}

/// Non-blocking readiness probe for [`read_from_raw_handle`]'s `STD_INPUT_HANDLE`: answers
/// "would a `read_from_raw_handle` call right now return immediately" without itself blocking or
/// consuming any input, mirroring what a real kernel's `poll(2)`/`select(2)` does for an
/// inherited stdin fd independently of the read path.
///
/// This exists because [`read_from_raw_handle`] can genuinely block indefinitely on a pipe/regular
/// file's direct `ReadFile` call: unlike a real Linux fd, Windows gives no portable way to make a
/// handle's `ReadFile` itself non-blocking or cancellable mid-call from another thread, so the
/// guest-visible `poll`/`select`/`epoll_wait` syscalls (see
/// `litebox_shim_linux::syscalls::epoll::EpollDescriptor::poll`'s `File` arm) must be answered by
/// a *separate*, genuinely non-blocking readiness check instead of by the read call itself. Before
/// this existed, that `poll` arm hardcoded stdin as always-readable, which is exactly wrong for a
/// real interactive console with no pending keystrokes: libuv (Node's stdio backend) polls stdin
/// as part of its startup/`uv_tty_init` path, saw the hardcoded "readable", issued a `read()`
/// that landed in the blocking `ReadFile` above, and hung forever with a genuinely-attached
/// console that had no pending input -- the process-never-exits bug this function fixes.
///
/// Dispatches on the handle's real type:
/// - **Console** (`FILE_TYPE_CHAR`): defers to [`ConsoleStdinReader::is_ready`] -- see its doc
///   comment for why the raw `INPUT_RECORD` queue (`GetNumberOfConsoleInputEvents`/
///   `PeekConsoleInputW`) cannot reliably answer this once `ENABLE_LINE_INPUT`'s cooked-read line
///   editor is involved, which a real interactive console always has enabled by default.
/// - **Pipe** (`FILE_TYPE_PIPE`, e.g. a redirected/piped stdin): `PeekNamedPipe` reports the
///   number of bytes currently available to read without consuming them (no cooked-read layer
///   applies to a pipe, so this remains a direct, reliable non-consuming probe).
/// - Anything else (regular file, `FILE_TYPE_UNKNOWN`, invalid/null handle): these are always
///   immediately readable (a disk file's `ReadFile` never blocks waiting for data to arrive, and
///   an absent handle behaves like already-closed/EOF stdin, matching [`read_from_raw_handle`]'s
///   own `Ok(0)` treatment of that case) -- report ready so the guest's `read()` promptly
///   observes the real outcome instead of appearing to hang on a readiness check.
fn stdin_ready_raw_handle(platform: &'static WindowsUserland) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_CHAR, FILE_TYPE_PIPE, GetFileType};
    use windows_sys::Win32::System::Console::STD_INPUT_HANDLE;
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    let handle = unsafe { windows_sys::Win32::System::Console::GetStdHandle(STD_INPUT_HANDLE) };
    if handle.is_null() || handle == Win32_Foundation::INVALID_HANDLE_VALUE {
        // No input attached at all: behaves like already-closed/EOF stdin (see
        // `read_from_raw_handle`'s identical handling), which is always "ready" to read (the
        // read immediately returns `Ok(0)`).
        return true;
    }

    match unsafe { GetFileType(handle) } {
        FILE_TYPE_CHAR => ConsoleStdinReader::get(platform).is_ready(),
        FILE_TYPE_PIPE => {
            let mut available: u32 = 0;
            let ok = unsafe {
                PeekNamedPipe(
                    handle,
                    core::ptr::null_mut(),
                    0,
                    core::ptr::null_mut(),
                    &raw mut available,
                    core::ptr::null_mut(),
                )
            };
            if ok == 0 {
                // A broken/closed pipe reads as EOF (see `read_from_raw_handle`'s
                // `ERROR_BROKEN_PIPE` handling) -- that is also "ready" (the read returns `Ok(0)`
                // immediately rather than blocking).
                return true;
            }
            available > 0
        }
        // Regular disk file, `FILE_TYPE_UNKNOWN`, or anything else: `ReadFile` never blocks
        // waiting for data on these, so they are always immediately ready.
        _ => true,
    }
}

/// Writes directly to the process's raw `STD_OUTPUT_HANDLE`/`STD_ERROR_HANDLE` via `WriteFile`,
/// bypassing `std::io::stdout()`/`std::io::stderr()`.
///
/// # Why not `std::io::stdout()`/`std::io::stderr()`
///
/// Every emulated Linux guest "process" litebox creates is, under the hood, an ordinary Windows
/// thread inside this single shared host process (see `spawn_thread`/`thread_start` above, and
/// `Vmem::duplicate`'s doc comment) -- there is no per-guest-process OS-level isolation for
/// anything that is itself process-global host state. `std::io::Stdout`/`std::io::Stdin` are
/// exactly such state: each is a lazily-initialized, process-wide singleton guarded by its own
/// internal lock (`ReentrantMutex` wrapping a `LineWriter`), shared by every caller in the host
/// process regardless of which guest "process" or thread is calling. On real Linux, by contrast,
/// two independent processes writing to the same (or different) fd 1 never contend on any
/// in-process Rust-level lock at all -- the kernel's own per-file-description state and the
/// `write(2)` syscall boundary provide all necessary serialization, and neither process can ever
/// be blocked by the other holding a lock inside libc.
///
/// This mismatch was investigated as a candidate cause of a real hang this session chased
/// (`sh -c "timeout 5 tar -tzf <2-member-gzip.tar.gz>"`, where two guest processes each
/// independently call `write(1, ...)`/`write(2, ...)` at nearly the same moment) -- syscall-level
/// tracing during that investigation caught one process's `writev(fd=1, ...)` as the last syscall
/// it ever issued, never returning, while a second process concurrently reached its own
/// stdout-bound write around the same instant, and `gdb`-attaching to the stalled process showed
/// every real OS thread cleanly parked (no panic, no spin) -- a pattern consistent with, though
/// not conclusively proven to be, one guest thread's `write()` becoming stuck on
/// `std::io::Stdout`'s internal lock (`ThreadHandle::interrupt`'s `SuspendThread`/`ResumeThread`
/// pair, used by both `fork_verify` and process-exit teardown, can suspend a thread while it is
/// executing arbitrary *host* Rust code, including while it holds that lock, since `SuspendThread`
/// is called unconditionally before this module's `is_in_guest` check -- an OS-level
/// thread-suspend primitive has no notion of "don't suspend while a Rust-level lock is held").
/// That specific hang was ultimately root-caused to a different, independently-confirmed bug (a
/// process-exit fd leak fixed in `litebox_shim_linux`'s `close_all_fds_on_process_exit`), so this
/// exact lock-contention scenario was not the deciding factor there -- but the underlying
/// coupling this fix removes is real and independently worth fixing on its own: a guest process
/// legitimately has no reason to ever be blockable by another, unrelated guest process's
/// console/pipe writes, and routing every guest "process"'s stdio through one shared host-level
/// lock creates exactly that illegitimate dependency regardless of whether this particular repro
/// exercises it.
///
/// The fix: skip `std::io`'s buffering/locking entirely and issue the write as a single raw
/// `WriteFile` call against the real OS handle, the same way a genuine Linux `write(2)` syscall
/// would go straight to the kernel with no intervening userspace lock. This is correct for both a
/// real console (`WriteFile` writes bytes through the console's active codepage, exactly as a
/// real Linux process's raw `write()` to an inherited console fd would) and a redirected
/// file/pipe (a plain byte-for-byte `WriteFile`).
///
/// One gap this leaves: real Linux `write(2)` to a TTY (or a pipe/regular file, up to
/// implementation-defined size limits -- `PIPE_BUF`-ish for pipes) is atomic with respect to other
/// concurrent writers to the *same* file description -- the kernel serializes byte ranges so one
/// writer's bytes are never torn/interleaved mid-flight with another's. A single guest "process"
/// can itself be multi-threaded (every guest thread is an ordinary Windows thread in this shared
/// host process, same as the guest-process note above), and Win32's `WriteFile` on a console
/// handle provides no equivalent atomicity guarantee across concurrent callers -- two threads
/// calling `WriteFile` on the same `STD_OUTPUT_HANDLE`/`STD_ERROR_HANDLE` at once can have their
/// bytes genuinely interleaved by the console subsystem, which a real Linux kernel would never
/// allow. This was the confirmed mechanism behind a reported live keystroke/output corruption bug
/// in a heavily-multithreaded guest (Node.js's REPL, whose main JS thread, libuv threadpool, and
/// V8 GC/compiler threads can all independently reach `write(1, ...)`/`write(2, ...)`): one
/// thread's diagnostic stderr write landed spliced into the middle of another thread's stdout
/// bytes.
///
/// Fixed with a pair of raw mutexes (`STDOUT_WRITE_LOCK`/`STDERR_WRITE_LOCK`, one per stream so a
/// stalled stdout writer never blocks a concurrent stderr writer or vice versa), held only for the
/// duration of the `WriteFile` call itself -- never across anything that can block indefinitely.
/// This is safe with respect to `ThreadHandle::interrupt`'s `SuspendThread`/`ResumeThread` pair
/// (the documented hazard above, where a thread suspended while holding a lock can wedge every
/// other thread waiting on it forever): `interrupt` always pairs its `SuspendThread` with a
/// `defer`-guaranteed `ResumeThread` before `interrupt` itself returns, so the suspend window is
/// bounded by that one function call, never indefinite -- a thread blocked on this lock waits out,
/// at worst, one `interrupt` call's short suspend/resume window, never forever. This differs from
/// the `std::io::Stdout` case in scope, not just mechanism: that lock was one process-wide
/// singleton shared by *every* guest process for *every* stdio stream, coupling unrelated guest
/// processes' liveness together; these locks are per-stream only, so unrelated guest
/// processes/threads writing to different streams never contend at all, and even same-stream
/// writers only ever wait for one bounded `WriteFile` call to finish.
static STDOUT_WRITE_LOCK: Mutex<()> = Mutex::new(());
static STDERR_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Serializes every `VirtualProtect` call this process issues against guest-mapped memory.
///
/// `VirtualProtect` changes are page-granular and process-wide: two threads racing independent
/// `VirtualProtect` calls that happen to cover the same page (e.g. an ordinary guest `mprotect()`
/// on one thread racing [`fork_verify`](crate::fork_verify)'s own temporary
/// read-only-to-`PAGE_EXECUTE_READWRITE`-and-back flip on another, unrelated, thread while healing
/// a stale post-`fork()` pointer slot) can interleave: one thread's `VirtualProtect(...,
/// old_protect_restore)` can land between another thread's `VirtualProtect(...,
/// PAGE_EXECUTE_READWRITE)` and its write, transiently narrowing the page back to a
/// non-writable/non-executable protection out from under an in-flight write or a concurrent
/// instruction fetch on a third thread executing code on the same page -- observed in practice as
/// a `STATUS_ACCESS_VIOLATION` on a small-offset near-null address (`addr=0x18`-shaped) on a
/// completely unrelated thread shortly after an unrelated fork-child's own post-exit healing pass
/// ran. Every call site that mutates page protection on guest-mapped memory (both
/// [`WindowsUserland::update_permissions`](crate) and
/// [`fork_verify::write_usize_fault_tolerant`](crate::fork_verify)) must hold this lock for the
/// full read-modify-write span (query/flip, mutate, restore), not just around the `VirtualProtect`
/// call itself, so the two paths can never observe or produce a torn intermediate protection state
/// on a shared page.
///
/// This is the SAME lock as [`ALLOCATE_PAGES_FIXED_ADDR_LOCK`] (a `const` alias, not a second
/// `Mutex`) -- they used to be two separate mutexes guarding the same underlying resource (the
/// process's Windows VAD tree, read via `VirtualQuery` and mutated via `VirtualProtect`/
/// `VirtualFree`/`VirtualAlloc2`), which meant a `VirtualProtect` here (from `update_permissions`
/// or `fork_verify::write_usize_fault_tolerant`) had NO mutual exclusion at all against a
/// concurrent `deallocate_pages`/`allocate_pages` region-walk on an overlapping or adjacent
/// region, even though BOTH locks existed for the identical reason (a multi-step query-then-
/// mutate span racing a concurrently-running guest thread's own VAD-mutating call). Confirmed
/// live as the root cause of a real, reproducible `labwc` SIGSEGV that only manifested under
/// concurrent multi-process load (`labwc` + `xfwm4` + `xfdesktop` running together, `xfwm4`'s own
/// thread independently confirmed to be hitting `fork_verify`'s `write_usize_fault_tolerant`
/// heavily and concurrently during the exact window `labwc`'s own `mmap`/`munmap` sequence
/// crashed in) -- `labwc`'s own single-threaded repro was already fully fixed by two earlier,
/// narrower locking fixes in this same file (see `process_memory_range_by_regions`'s and
/// `allocate_pages`'s own doc comments/history), but this specific concurrent-load crash
/// persisted through both, because neither closed the real remaining gap: TWO different locks
/// protecting the SAME shared Windows resource is equivalent to no lock at all between the two
/// code paths that each hold a different one.
pub(crate) static VIRTUAL_PROTECT_LOCK: Mutex<()> = Mutex::new(());

/// Every live mapping of each shared-memory handle, so all views of one object can
/// be sampled at the SAME instant (see `diag-shm-crossview`). Diagnostic only.
static ADV_SHM_VIEWS: Mutex<std::collections::BTreeMap<usize, Vec<(usize, usize)>>> =
    Mutex::new(std::collections::BTreeMap::new());

/// Serializes the ENTIRE `fork_verify` healing sequence (every AV-path healer plus
/// `on_single_step`) across threads process-wide.
///
/// # Why this exists
///
/// Confirmed live (this investigation, passes 168-172): two guest threads spawned from the same
/// fork group (sharing the exact same relocation-tracked pages) can be mid-healing at genuinely
/// the same moment -- `ThreadId(16)`/`ThreadId(17)` both actively patching stale slots in the
/// exact window a THIRD thread's `sys_write` syscall prologue faulted on a plain, unrelated
/// stack-relative store. `write_usize_fault_tolerant`'s own `VIRTUAL_PROTECT_LOCK` only serializes
/// this module's writes against `VirtualProtect`/each other -- it says nothing about the READ
/// side of the read-decide-write sequence in each AV-path/single-step case, nor about a
/// completely unrelated thread's ordinary (non-`fork_verify`) instruction fetch/data access
/// racing a healer's in-flight page-protection flip. Making the individual slot read/write atomic
/// (see `read_usize_fault_tolerant`'s own doc comment) closed the narrowest torn-word hazard but
/// did not close the broader one: two healers independently walking the SAME instruction's
/// operands/registers/decode state at once is not sound just because each individual memory
/// access is atomic. Serializing the whole healing sequence per-fault removes that broader
/// class of concurrent-healer hazard entirely, at the cost of one thread's healing work being
/// briefly delayed (never blocked long -- healing is always a small, bounded, non-blocking
/// sequence of local checks and at most one word write) while another thread's healing for a
/// DIFFERENT fault runs.
///
/// Uses `try_lock` in a small bounded spin (never an unconditional blocking `.lock()`): this runs
/// inside exception-handling context, where a genuine nested re-entry on the SAME thread is a
/// documented, observed scenario elsewhere in this file (see `TlsState::veh_depth`'s doc comment)
/// -- an unconditional lock would deadlock a thread against itself on that path. A failed
/// acquisition after the bounded spin falls through to the pre-existing, unserialized behavior
/// (strictly no worse than before this fix) rather than risking an indefinite block inside VEH
/// dispatch.
pub(crate) static FORK_VERIFY_HEAL_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    /// How many times THIS thread currently holds [`FORK_VERIFY_HEAL_LOCK`], so a nested fault on
    /// the same thread can recognise its own ownership instead of deadlocking against itself.
    static FORK_VERIFY_HEAL_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// A held (or re-entrantly observed) [`FORK_VERIFY_HEAL_LOCK`].
pub(crate) struct ForkVerifyHealGuard {
    /// `None` when this thread already held the lock further up its own stack.
    _outer: Option<std::sync::MutexGuard<'static, ()>>,
}

impl Drop for ForkVerifyHealGuard {
    fn drop(&mut self) {
        FORK_VERIFY_HEAL_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Acquire [`FORK_VERIFY_HEAL_LOCK`], blocking for OTHER threads and re-entering freely on this
/// one.
///
/// # Why this replaced a bounded-spin `try_lock`
///
/// The previous acquisition spun `try_lock` a thousand times and, on failure, fell through to
/// "the pre-existing, unserialized behavior (strictly no worse than before this fix)". But
/// unserialized healing is precisely the hazard this lock exists to prevent -- its own doc comment
/// records two threads confirmed mid-healing at the same moment, and says "two healers
/// independently walking the SAME instruction's operands/registers/decode state at once is not
/// sound just because each individual memory access is atomic". A give-up path that then does the
/// unsound thing turns the contended case -- the only case the lock matters in -- into the broken
/// one. `mate-session` runs dozens of threads and contends this constantly.
///
/// Blocking was avoided for a real reason: a nested fault on the SAME thread would deadlock
/// against itself (see `TlsState::veh_depth`). That is a re-entrancy problem, not a reason to
/// abandon mutual exclusion, and re-entrancy is what this fixes -- a per-thread depth count lets
/// this thread recognise a lock it already owns and proceed, while a DIFFERENT thread still waits
/// its turn. Waiting is bounded in practice: healing is, by the same doc comment, "a small,
/// bounded, non-blocking sequence of local checks and at most one word write".
pub(crate) fn lock_fork_verify_heal_reentrant() -> ForkVerifyHealGuard {
    let already_held = FORK_VERIFY_HEAL_DEPTH.with(|d| {
        let previous = d.get();
        d.set(previous.saturating_add(1));
        previous > 0
    });
    let outer = if already_held {
        None
    } else {
        Some(
            FORK_VERIFY_HEAL_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    };
    ForkVerifyHealGuard { _outer: outer }
}

/// Serializes `allocate_pages`'s fixed-address (`suggested_range.start != 0`) check-then-act
/// sequence -- see that call site's own comment for the real TOCTOU race this closes: without a
/// lock spanning the ENTIRE query-then-allocate span, two concurrently-running guest processes'
/// own OS threads could both observe the same real address range as free and then both allocate
/// into it, silently aliasing each other's memory with no error, no page fault, and no
/// guest-visible signal -- the exact same class of gap `CLAIMED_RANGES`'s own doc comment
/// documents for a DIFFERENT, already-defended case (fixed-address `Replace`-mode reuse); this
/// lock closes the general case for every `fixed_address_behavior` variant.
///
/// This is a `const` alias for [`VIRTUAL_PROTECT_LOCK`], not a second `Mutex` -- see that
/// constant's own doc comment for why unifying the two was necessary: both guard the same
/// underlying resource (the process's shared Windows VAD tree), and having them be separate
/// mutexes meant `VirtualProtect` (protected only by `VIRTUAL_PROTECT_LOCK`) and
/// `VirtualFree`/`VirtualAlloc2` region-walks (protected only by this lock) had zero mutual
/// exclusion against each other despite both mutating/reading the same shared kernel structure.
const ALLOCATE_PAGES_FIXED_ADDR_LOCK: &Mutex<()> = &VIRTUAL_PROTECT_LOCK;

fn write_to_raw_handle(
    handle: windows_sys::Win32::Foundation::HANDLE,
    buf: &[u8],
    lock: &Mutex<()>,
) -> Result<usize, litebox::platform::StdioWriteError> {
    use windows_sys::Win32::Storage::FileSystem::WriteFile;

    if handle.is_null() || handle == Win32_Foundation::INVALID_HANDLE_VALUE {
        // No console/redirected output attached at all: silently discard, matching a Linux
        // process whose stdout/stderr fd was closed out from under it (further writes are
        // simply lost from the caller's perspective once the peer is gone) rather than panicking.
        return Ok(buf.len());
    }
    let mut written: u32 = 0;
    let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    // Serialize this write against any other concurrent writer to the same stream (see the doc
    // comment above); held only across the `WriteFile` call itself, never across anything that can
    // block for an unbounded time.
    let _guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ok = unsafe {
        WriteFile(
            handle,
            buf.as_ptr(),
            len,
            &raw mut written,
            core::ptr::null_mut(),
        )
    };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        // The reader end of a redirected pipe going away is the Windows analogue of `EPIPE`/a
        // broken pipe on Linux -- report it the same way the previous `std::io`-based
        // implementation did, rather than panicking.
        if err == Win32_Foundation::ERROR_BROKEN_PIPE || err == Win32_Foundation::ERROR_NO_DATA {
            return Err(litebox::platform::StdioWriteError::Closed);
        }
        panic!("WriteFile(stdio handle) failed: error={err}");
    }
    Ok(written as usize)
}

impl litebox::platform::StdioProvider for WindowsUserland {
    fn read_from_stdin(&self, buf: &mut [u8]) -> Result<usize, litebox::platform::StdioReadError> {
        read_from_raw_handle(self.as_static(), buf)
    }

    fn write_to(
        &self,
        stream: litebox::platform::StdioOutStream,
        buf: &[u8],
    ) -> Result<usize, litebox::platform::StdioWriteError> {
        use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};

        let (std_handle, lock) = match stream {
            litebox::platform::StdioOutStream::Stdout => (STD_OUTPUT_HANDLE, &STDOUT_WRITE_LOCK),
            litebox::platform::StdioOutStream::Stderr => (STD_ERROR_HANDLE, &STDERR_WRITE_LOCK),
        };
        let handle = unsafe { windows_sys::Win32::System::Console::GetStdHandle(std_handle) };
        write_to_raw_handle(handle, buf, lock)
    }

    fn is_a_tty(&self, stream: litebox::platform::StdioStream) -> bool {
        use litebox::platform::StdioStream;
        use std::io::IsTerminal as _;
        match stream {
            StdioStream::Stdin => std::io::stdin().is_terminal(),
            StdioStream::Stdout => std::io::stdout().is_terminal(),
            StdioStream::Stderr => std::io::stderr().is_terminal(),
        }
    }

    fn stdin_ready(&self) -> bool {
        stdin_ready_raw_handle(self.as_static())
    }

    fn tty_window_size(&self) -> Option<(u16, u16)> {
        use windows_sys::Win32::System::Console::{
            CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, STD_OUTPUT_HANDLE,
        };

        let handle =
            unsafe { windows_sys::Win32::System::Console::GetStdHandle(STD_OUTPUT_HANDLE) };
        if handle.is_null() || handle == Win32_Foundation::INVALID_HANDLE_VALUE {
            return None;
        }
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { core::mem::zeroed() };
        if unsafe { GetConsoleScreenBufferInfo(handle, &raw mut info) } == 0 {
            // Not a real console (e.g. redirected stdout): let the caller fall back to a
            // reasonable default rather than reporting a fake size.
            return None;
        }
        // `srWindow` is the visible window rectangle, not the full (possibly larger, scrollback-
        // including) screen buffer size -- this matches what a real Linux tty's `TIOCGWINSZ`
        // reports: the visible terminal dimensions, not a scrollback buffer size.
        let cols = info
            .srWindow
            .Right
            .saturating_sub(info.srWindow.Left)
            .saturating_add(1);
        let rows = info
            .srWindow
            .Bottom
            .saturating_sub(info.srWindow.Top)
            .saturating_add(1);
        let cols = u16::try_from(cols).ok()?;
        let rows = u16::try_from(rows).ok()?;
        if cols == 0 || rows == 0 {
            return None;
        }
        Some((rows, cols))
    }
}

/// **Reverted to private per-process backing 2026-09-17** (selective-routing correction, see
/// [`WindowsUserland::alloc`]'s own doc comment for the full story): this is now backed by the
/// ORDINARY per-call, per-process `VirtualAlloc2` mechanism again, exactly as it was before Track B
/// step 3 (`c08182d`). Routing EVERY host-heap allocation through the fixed-base shared kernel heap
/// (`c08182d` through `3d661d2`) proved not viable live: an ordinary one-shot allocation (e.g. the
/// OCI rootfs-reconstruction buffer every plain guest exec makes) is indistinguishable, at this
/// choke point, from genuinely-must-be-shared kernel state, and the shared section is a bump
/// allocator with no reclaim -- routing everything through it exhausted the 8 GiB reservation after
/// 45-90 real execs under `webtop_stack.sh` (`memory allocation of 181493744 bytes failed`, live,
/// 218 occurrences). The shared-kernel-heap machinery itself (fixed base, atomic cross-process
/// cursor, handle export/inherit) is NOT deleted -- see [`shared_kernel_arena_alloc`] below, now a
/// small, bounded, STANDALONE bump arena deliberately not wired to `GlobalAlloc`/this
/// `#[global_allocator]` at all, reserved for a follow-up session's `LiteBoxX`/`GlobalState`-only
/// migration (`AGENTS.md`, "Fixed-base shared kernel heap" section).
#[global_allocator]
static SLAB_ALLOC: litebox::mm::allocator::SafeZoneAllocator<'static, 34, WindowsUserland> =
    litebox::mm::allocator::SafeZoneAllocator::new();

/// Temporary, allocation-free diagnostic for the long-standing deterministic `rax =
/// HOST_ALLOCATOR_REGION_MIN + 0x1013480` leak (see this file's `HOST_ALLOCATOR_REGION_MIN` doc
/// comment and the investigation notes it links to). Gated behind `LITEBOX_DIAG_ALLOC=1`; logs
/// every `WindowsUserland::alloc` call's returned base address and size via a raw `OutputDebugStringA`
/// call so it cannot recurse into this same allocator (unlike `eprintln!`/`format!`, which route
/// through allocating machinery elsewhere in the host Rust runtime). Remove once the leak is
/// root-caused and fixed.
static DIAG_ALLOC_COUNT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Tri-state cache for [`diag_alloc_enabled`]: 0 = not yet checked, 1 = enabled, 2 = disabled.
/// Populated lazily, on the FIRST call, directly via the raw `GetEnvironmentVariableA` Win32 API
/// rather than `std::env::var_os` -- `var_os` allocates an `OsString`, which would recurse into
/// this very allocator when called from `WindowsUserland::alloc` itself (observed in practice to
/// hang: reentering `OnceLock`'s internal synchronization on the same thread self-deadlocks, and a
/// naive allocating re-check on every call is no better, just slower to hang). `GetEnvironmentVariableA`
/// writes into a fixed-size stack buffer and touches no Rust allocator at all, so it is safe to call
/// from inside `alloc` on the very first host allocation the process ever makes -- unlike the
/// earlier `init`-called-from-`WindowsUserland::new` approach, this has no blind spot for
/// allocations made before that constructor runs (e.g. `tracing_subscriber`/`clap`/the mmapped
/// rootfs tar setup in `litebox_runner_linux_on_windows_userland::run`, all of which allocate via
/// this same global allocator before `Platform::new()` is ever reached).
static DIAG_ALLOC_ENABLED_CACHE: core::sync::atomic::AtomicU8 =
    core::sync::atomic::AtomicU8::new(0);

/// Allocation-free: reads `LITEBOX_DIAG_ALLOC` via the raw Win32 API on first call (caching the
/// result), or just the cache thereafter. Safe to call from inside `WindowsUserland::alloc` itself,
/// including the very first allocation the process ever makes.
fn diag_alloc_enabled() -> bool {
    let cached = DIAG_ALLOC_ENABLED_CACHE.load(Ordering::Relaxed);
    if cached != 0 {
        return cached == 1;
    }
    let mut buf = [0u8; 4];
    let name = b"LITEBOX_DIAG_ALLOC\0";
    let len = unsafe {
        windows_sys::Win32::System::Environment::GetEnvironmentVariableA(
            name.as_ptr(),
            buf.as_mut_ptr(),
            4u32,
        )
    };
    // `GetEnvironmentVariableA` returns 0 (with `GetLastError() ==
    // ERROR_ENVIRONMENT_VARIABLE_NOT_FOUND`) when the variable is unset -- any successful return
    // (the variable exists, regardless of its value) is enough to enable this diagnostic, matching
    // every other `LITEBOX_*`-gated diagnostic in this file (`std::env::var_os(..).is_some()`).
    let enabled = len != 0;
    DIAG_ALLOC_ENABLED_CACHE.store(if enabled { 1 } else { 2 }, Ordering::Relaxed);
    enabled
}

/// Is a `LITEBOX_*` environment variable set, read without allocating?
///
/// `std::env::var_os` allocates an `OsString`, which is not safe on a fault path (and, from inside
/// the allocator itself, recurses -- see `diag_alloc_enabled`'s own doc comment for the hang that
/// caused). This reads the raw Win32 API instead. Uncached, because the one caller runs at most
/// once per process.
///
/// `name` must be NUL-terminated.
fn raw_env_is_set(name: &[u8]) -> bool {
    let mut buf = [0u8; 4];
    // A return of 0 means "not found"; any other value means the variable exists, whatever its
    // value -- matching how every other `LITEBOX_*` diagnostic gate in this file treats presence.
    0 != unsafe {
        windows_sys::Win32::System::Environment::GetEnvironmentVariableA(
            name.as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
    }
}

/// Allocation-free: reads a `LITEBOX_INTERNAL_FORK_CHILD_*` environment variable set by a
/// cross-process-fork PARENT (see [`FORK_CHILD_SHARED_HEAP_SECTION_ENV_VAR`]/
/// [`FORK_CHILD_SHARED_HEAP_BASE_ENV_VAR`]) and parses it as an unsigned decimal `usize`, or
/// returns `None` if unset, empty, too long for the fixed buffer, or not all-decimal-digits. Uses
/// the same raw `GetEnvironmentVariableA` mechanism [`diag_alloc_enabled`]/[`raw_env_is_set`]
/// already rely on for the identical allocation-free-on-the-process's-very-first-host-allocation
/// constraint -- see [`diag_alloc_enabled`]'s doc comment for why `std::env::var`/`var_os` cannot
/// be used here instead. `name` must be NUL-terminated.
fn raw_env_read_usize(name: &[u8]) -> Option<usize> {
    let mut buf = [0u8; 24];
    let len = unsafe {
        windows_sys::Win32::System::Environment::GetEnvironmentVariableA(
            name.as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
    };
    if len == 0 || len as usize >= buf.len() {
        return None;
    }
    let mut value: usize = 0;
    for &b in &buf[..len as usize] {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(usize::from(b - b'0'))?;
    }
    Some(value)
}

/// Set once the process has written (or failed to write) its crash dump, so a fault cascade cannot
/// try again from a second thread while the first attempt is still running.
static CRASH_DUMP_ATTEMPTED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Write a Windows minidump for an unrecoverable fault, via the OS's own `MiniDumpWriteDump`.
///
/// # Why this exists
///
/// Everything this crate previously left behind for a fatal host-side fault was the allocation-free
/// `[diag-unrecov-av-*]` ring dump: a faulting `rip`, a handful of stack words, and the module
/// RVAs. Those are only meaningful against the exact build that emitted them, so a log outliving
/// one rebuild becomes unsymbolizable -- and symbolizing it against a newer binary yields
/// confident, wrong names (see `advisor/probes/symbolize_litebox_crash.py`, which exists because
/// this was done by hand, and which had to grow a warning about exactly that). A minidump carries
/// its own module list with build identities, every thread's stack, and the memory those stacks
/// reference, so it stays analysable for as long as the matching `.pdb` exists.
///
/// AGENTS.md's `RtlpUnwindPrologue` section lists integrating Mozilla's `minidump-writer` crate as
/// a next step for exactly this. No crate is needed: `MiniDumpWriteDump` is the native API that
/// crate wraps, `windows-sys` already declares it, and this crate already enables both features it
/// needs (`Win32_System_Diagnostics_Debug`, `Win32_System_Kernel`). Linking it costs a load-time
/// dependency on `dbghelp.dll`, which is deliberate: the alternative is a lazy `LoadLibrary` from
/// inside a fault handler, which can deadlock against the loader lock the faulting thread may
/// already hold.
///
/// # What it deliberately does NOT capture
///
/// Not `MiniDumpWithFullMemory`. A litebox process maps the guest's entire address space -- many
/// gigabytes for a desktop -- and a full-memory dump of that is both unusable and liable to fill
/// the disk at the worst possible moment. `MiniDumpWithThreadInfo |
/// MiniDumpWithIndirectlyReferencedMemory | MiniDumpWithUnloadedModules` captures every thread's
/// stack plus the memory those stacks point at, which is what reconstructs a call chain and shows
/// what a faulting pointer referenced, at a few MB.
///
/// # Ordering and failure
///
/// Called AFTER the fault-terminate watchdog is armed, on purpose. `MiniDumpWriteDump` walks every
/// thread in the process and can block; if it never returns, the watchdog still kills the process,
/// so the dump attempt can never turn a crash into a hang. Every step is best-effort and silent on
/// failure beyond one diagnostic line -- the process is dying either way, and the ring dump above
/// has already been emitted.
fn write_crash_minidump(exception_info: *mut EXCEPTION_POINTERS) {
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_ALWAYS, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, GetTempPathW,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{
        MINIDUMP_EXCEPTION_INFORMATION, MiniDumpWithIndirectlyReferencedMemory,
        MiniDumpWithThreadInfo, MiniDumpWithUnloadedModules, MiniDumpWriteDump,
    };

    if CRASH_DUMP_ATTEMPTED.swap(true, Ordering::SeqCst) {
        return;
    }
    if raw_env_is_set(b"LITEBOX_NO_CRASH_DUMP\0") {
        return;
    }

    let pid = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessId() };

    // Path built into a fixed stack buffer: `<temp>\litebox-crash-<pid>.dmp`. No allocation, for
    // the same reason the ring dump allocates nothing -- the heap is not trustworthy here, and on
    // a stack-exhaustion fault neither is a large frame.
    let mut path = [0u16; 320];
    let temp_len = unsafe {
        GetTempPathW(
            path.len() as u32 - 64,
            path.as_mut_ptr(),
        )
    } as usize;
    // `GetTempPathW` returns 0 on failure; fall back to the current directory, which is always a
    // legal relative path, rather than giving up on the dump entirely.
    let mut pos = if temp_len == 0 || temp_len >= path.len() - 64 {
        0
    } else {
        temp_len
    };
    for ch in "litebox-crash-".encode_utf16() {
        path[pos] = ch;
        pos += 1;
    }
    // Decimal pid, written most-significant digit first without `format!`.
    let mut digits = [0u16; 10];
    let mut n = pid;
    let mut d = 0usize;
    if n == 0 {
        digits[0] = u16::from(b'0');
        d = 1;
    }
    while n > 0 {
        digits[d] = u16::from(b'0') + u16::try_from(n % 10).unwrap_or(0);
        n /= 10;
        d += 1;
    }
    while d > 0 {
        d -= 1;
        path[pos] = digits[d];
        pos += 1;
    }
    for ch in ".dmp".encode_utf16() {
        path[pos] = ch;
        pos += 1;
    }
    path[pos] = 0;

    let file = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ,
            core::ptr::null(),
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            core::ptr::null_mut(),
        )
    };
    if file.is_null() || file == INVALID_HANDLE_VALUE {
        diag_raw_print(
            b"[diag-crash-dump] CreateFileW failed, no dump written for pid=",
            pid as usize,
            b" err=0x",
            unsafe { GetLastError() } as usize,
        );
        return;
    }

    let mut info = MINIDUMP_EXCEPTION_INFORMATION {
        ThreadId: unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() },
        ExceptionPointers: exception_info,
        // The exception pointers are in THIS process's address space, not a client's.
        ClientPointers: 0,
    };
    let ok = unsafe {
        MiniDumpWriteDump(
            GetCurrentProcess(),
            pid,
            file,
            MiniDumpWithThreadInfo
                | MiniDumpWithIndirectlyReferencedMemory
                | MiniDumpWithUnloadedModules,
            &raw const info,
            core::ptr::null(),
            core::ptr::null(),
        )
    };
    let _ = &mut info;
    unsafe { CloseHandle(file) };
    // The pid is the filename, so printing it is enough to locate the dump -- and it avoids
    // pushing a UTF-16 path through the byte-oriented raw printer on a dying process.
    diag_raw_print(
        b"[diag-crash-dump] wrote %TEMP%/litebox-crash-<pid>.dmp pid=",
        pid as usize,
        b" ok=0x",
        usize::from(ok != 0),
    );
}

/// Format `n` as decimal into `buf`, returning the written prefix. No heap allocation.
fn fmt_usize_hex(mut n: usize, buf: &mut [u8; 20]) -> &[u8] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut i = buf.len();
    if n == 0 {
        i -= 1;
        buf[i] = b'0';
    } else {
        while n != 0 {
            i -= 1;
            buf[i] = DIGITS[n & 0xf];
            n >>= 4;
        }
    }
    &buf[i..]
}

/// Raw, allocation-free debug print: writes `prefix` + hex(`a`) + `mid` + hex(`b`) + "\n" straight
/// to the process's `STD_ERROR_HANDLE` via `WriteFile`, entirely on the stack, so CI's captured
/// stderr picks it up exactly like every other `LITEBOX_VEH_TRACE`-style diagnostic. Safe to call
/// from inside the global allocator itself since it never touches `SLAB_ALLOC` (unlike
/// `eprintln!`/`format!`, which can recurse into it via the host Rust I/O stack). Deliberately
/// skips `STDERR_WRITE_LOCK` -- interleaving with other stderr writers is an acceptable, purely
/// cosmetic risk for this temporary, allocation-free diagnostic.
// TEMPORARY diagnostic (litebox investigation: XFCE/weston mallocng heap-corruption bug
// hunt) -- wires up `litebox::mm::exception_table`'s memcpy-write watch range from the
// `LITEBOX_MEMCPY_WATCH=<start_hex>-<end_hex>` env var, logging any overlapping write via
// the same raw, allocation-free `diag_raw_print` mechanism the VEH diagnostics use. Lets a
// repro determine whether a specific guest heap address is ever written to via litebox's
// own fallible-memory-write path (any syscall copying host data into guest memory), as
// opposed to a raw guest-code store that never goes through litebox at all. Remove once the
// investigation concludes.
pub fn install_memcpy_watch_from_env() {
    let Some(spec) = std::env::var_os("LITEBOX_MEMCPY_WATCH") else {
        return;
    };
    let Some(spec) = spec.to_str() else { return };
    let Some((start_str, end_str)) = spec.split_once('-') else {
        return;
    };
    let (Ok(start), Ok(end)) = (
        usize::from_str_radix(start_str.trim_start_matches("0x"), 16),
        usize::from_str_radix(end_str.trim_start_matches("0x"), 16),
    ) else {
        return;
    };
    fn hook(dst: usize, size: usize) {
        diag_raw_print(b"[memcpy-watch] dst=0x", dst, b" size=0x", size);
    }
    unsafe {
        litebox::mm::exception_table::set_memcpy_watch_range(start, end, Some(hook));
    }
}

fn diag_raw_print(prefix: &[u8], a: usize, mid: &[u8], b: usize) {
    let mut line = [0u8; 128];
    let mut pos = 0usize;
    let push = |bytes: &[u8], line: &mut [u8; 128], pos: &mut usize| {
        let n = bytes.len().min(line.len().saturating_sub(*pos));
        line[*pos..*pos + n].copy_from_slice(&bytes[..n]);
        *pos += n;
    };
    push(prefix, &mut line, &mut pos);
    let mut hexbuf = [0u8; 20];
    push(fmt_usize_hex(a, &mut hexbuf), &mut line, &mut pos);
    push(mid, &mut line, &mut pos);
    let mut hexbuf2 = [0u8; 20];
    push(fmt_usize_hex(b, &mut hexbuf2), &mut line, &mut pos);
    push(b"\n", &mut line, &mut pos);
    unsafe {
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
        let handle = GetStdHandle(STD_ERROR_HANDLE);
        if !handle.is_null() && handle != Win32_Foundation::INVALID_HANDLE_VALUE {
            let mut written: u32 = 0;
            windows_sys::Win32::Storage::FileSystem::WriteFile(
                handle,
                line.as_ptr(),
                u32::try_from(pos).unwrap_or(u32::MAX),
                &raw mut written,
                core::ptr::null_mut(),
            );
        }
    }
}

/// Raw, allocation-free, lock-free dump of the fault's full register set plus the exception
/// code/faulting address, via the same `WriteFile`-on-stack mechanism as [`diag_raw_print`].
/// Exists because `eprintln!`/`format!` (used by the `[veh-regs] ENTRY` print a few lines below
/// this call site) do real host heap allocation and take the stdio lock -- both were caught this
/// pass re-faulting on a thread whose heap/lock state is already corrupted by the same bug this
/// diagnostic exists to observe, silently losing the first, real, causative fault's own register
/// state behind a second "fault-in-the-fault-handler" (a non-standard `code=0x6` host-side
/// exception) every time it happened. Called as the LITERAL FIRST operation once the fatal-dump
/// gate is true, before even `VehDiagBlockGuard::enter()` -- if thread-local access or heap
/// allocation is what's re-faulting, guarding against re-entrancy after already attempting one of
/// those is too late.
#[allow(clippy::too_many_arguments, reason = "raw diagnostic dump, one field per register")]
fn diag_raw_regdump(
    code: u32,
    addr: usize,
    rip: usize,
    rax: usize,
    rbx: usize,
    rcx: usize,
    rdx: usize,
    rsi: usize,
    rdi: usize,
    rsp: usize,
    rbp: usize,
) {
    let mut line = [0u8; 512];
    let mut pos = 0usize;
    let push = |bytes: &[u8], line: &mut [u8; 512], pos: &mut usize| {
        let n = bytes.len().min(line.len().saturating_sub(*pos));
        line[*pos..*pos + n].copy_from_slice(&bytes[..n]);
        *pos += n;
    };
    let push_hex = |v: usize, line: &mut [u8; 512], pos: &mut usize| {
        let mut hexbuf = [0u8; 20];
        push(fmt_usize_hex(v, &mut hexbuf), line, pos);
    };
    push(b"[veh] RAWREGS tid=", &mut line, &mut pos);
    push_hex(
        unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() } as usize,
        &mut line,
        &mut pos,
    );
    push(b" code=", &mut line, &mut pos);
    push_hex(code as usize, &mut line, &mut pos);
    push(b" addr=", &mut line, &mut pos);
    push_hex(addr, &mut line, &mut pos);
    push(b" rip=", &mut line, &mut pos);
    push_hex(rip, &mut line, &mut pos);
    push(b" rax=", &mut line, &mut pos);
    push_hex(rax, &mut line, &mut pos);
    push(b" rbx=", &mut line, &mut pos);
    push_hex(rbx, &mut line, &mut pos);
    push(b" rcx=", &mut line, &mut pos);
    push_hex(rcx, &mut line, &mut pos);
    push(b" rdx=", &mut line, &mut pos);
    push_hex(rdx, &mut line, &mut pos);
    push(b" rsi=", &mut line, &mut pos);
    push_hex(rsi, &mut line, &mut pos);
    push(b" rdi=", &mut line, &mut pos);
    push_hex(rdi, &mut line, &mut pos);
    push(b" rsp=", &mut line, &mut pos);
    push_hex(rsp, &mut line, &mut pos);
    push(b" rbp=", &mut line, &mut pos);
    push_hex(rbp, &mut line, &mut pos);
    push(b"\n", &mut line, &mut pos);
    unsafe {
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
        let handle = GetStdHandle(STD_ERROR_HANDLE);
        if !handle.is_null() && handle != Win32_Foundation::INVALID_HANDLE_VALUE {
            let mut written: u32 = 0;
            windows_sys::Win32::Storage::FileSystem::WriteFile(
                handle,
                line.as_ptr(),
                u32::try_from(pos).unwrap_or(u32::MAX),
                &raw mut written,
                core::ptr::null_mut(),
            );
        }
    }
}

/// Fixed base address for the cross-process shared kernel arena (`advisor/ADVISORY-002-d-zero-fork.md`
/// section 3.3, Track B step 3). **NOT `SLAB_ALLOC`'s backing store as of 2026-09-17** -- see
/// [`SLAB_ALLOC`]'s and [`WindowsUserland::alloc`]'s doc comments for why routing EVERY host-heap
/// allocation through here was reverted (live-demonstrated shared-pool exhaustion after 45-90 real
/// execs). This region is now reserved exclusively for [`shared_kernel_arena_alloc`], a small,
/// standalone bump arena NOT wired to `GlobalAlloc`, intended for the specific, bounded set of
/// genuinely-must-be-cross-process-visible kernel singletons (`LiteBoxX`, `GlobalState`) a
/// follow-up session migrates onto it one type at a time (see that function's doc comment for the
/// remaining `SharedArc<T>`-style wrapper work this needs before any real caller can use it for
/// those types). A second process that maps the SAME pagefile-backed section at this SAME fixed
/// address would see byte-identical contents at byte-identical addresses, which is the
/// precondition every pointer inside that state (an `Arc`'s data pointer, a `BTreeMap` node
/// pointer, a `Vec`'s buffer pointer) needs to remain valid across the process boundary.
///
/// Placed a full 32 GiB above [`HOST_ALLOCATOR_REGION_MIN`] (that constant's own floating
/// `VirtualAlloc2`-per-call region is superseded by this one -- see [`WindowsUserland::alloc`] --
/// but is left in place unchanged as the guest `Vmem` partition boundary via `TASK_ADDR_MAX`, to
/// minimize blast radius) so the two regions cannot practically collide within any single
/// process's lifetime, while both stay well below the real usermode VA ceiling on 64-bit Windows
/// (~`0x7FFF_FFFE_FFFF`). This is NOT a proven collision-free band -- no such guarantee is
/// documented for any high-VA region on this platform (see the advisory's own "Reserve size and
/// placement" note, and this file's `spawn_exec_collision_child`/python3 collision investigation
/// for why that matters in practice). [`init_shared_kernel_heap`] verifies the actual returned
/// address at runtime and panics loudly on any mismatch rather than silently drifting, matching
/// this file's existing `spawn_cross_process_fork_child`/`copy_one_group` precedent of treating
/// an unexpected placement as fatal, never a soft relocate.
const SHARED_KERNEL_HEAP_BASE: usize = 0x7FF8_0000_0000;

/// Reserve size for the shared kernel heap: 8 GiB. **Live-corrected 2026-09-17** (full
/// `webtop_stack.sh` boot under `LITEBOX_PROCESS_FORK=1`, PRD
/// `shared-kernel-heap-eager-full-commit-not-lazy-reserve`): the earlier doc comment here claimed
/// a plain `CreateFileMappingW(INVALID_HANDLE_VALUE, ...)` pagefile-backed section "only commits
/// pages ... on first touch" -- that is FALSE for a section created without `SEC_RESERVE`.
/// Windows charges the FULL size against system commit limit at `CreateFileMappingW` time
/// (`SEC_COMMIT` is the implicit default), not lazily. Every cross-process-fork child calls
/// [`init_shared_kernel_heap`] on its own first allocation while the parent's own section is
/// still live, so real fork density (nginx's own crash-retry loop alone forks up to 30 times)
/// multiplies this into N x 8 GiB of commit charge, hitting `ERROR_COMMITMENT_LIMIT` at ~96% host
/// commit charge -- confirmed live. `SEC_RESERVE` (reserving the address range with no commit
/// charge up front) was the first fix attempted, but this exact fixed address rejects EVERY
/// reserve-only mapping mechanism tried (plain `SEC_RESERVE` section view, `MEM_RESERVE`
/// allocation type, the documented `MEM_RESERVE_PLACEHOLDER`/`MEM_REPLACE_PLACEHOLDER` pair, and
/// even a plain section-free `VirtualAlloc2(MEM_RESERVE)` -- all `ERROR_INVALID_ADDRESS`, live
/// confirmed, the last one even from an unrelated process) while only a genuinely `SEC_COMMIT`
/// section view succeeds there -- see [`init_shared_kernel_heap`]'s own doc comment for the full
/// elimination trail. Fixed instead by creating the section exactly as before (real `SEC_COMMIT`,
/// which succeeds at this address) and immediately `VirtualFree(..., MEM_DECOMMIT)`-ing the whole
/// freshly-mapped view before any other thread in the process can touch it, releasing that eager
/// commit charge right away; [`WindowsUserland::alloc`] then commits only the exact sub-range it
/// just bump-allocated via `VirtualAlloc2(..., MEM_COMMIT, ...)`, matching this codebase's own
/// established lazy-commit pattern for other large reservations. This constant still only bounds
/// how large the process's entire heap can ever grow, not steady-state memory
/// use; if the process ever needs more than 8 GiB of live heap, [`WindowsUserland::alloc`]'s
/// exhaustion path (`None`) surfaces as an ordinary allocator OOM. True cross-process SHARING of
/// this section's contents (a child mapping the PARENT's own existing section instead of
/// reserving its own) remains deliberately deferred to step 4 -- see `SHARED_KERNEL_HEAP_BASE`'s
/// doc comment on why cross-process vtable validity needs same-base loading first; today's fix
/// only makes each process's OWN reservation lazy, not shared.
///
/// **Shrunk from 8 GiB to 64 MiB, 2026-09-17** (selective-routing correction): this region no
/// longer backs `SLAB_ALLOC` (every host-heap allocation) -- see [`SLAB_ALLOC`]'s doc comment --
/// it is reserved solely for [`shared_kernel_arena_alloc`]'s small, bounded set of genuinely
/// cross-process-visible kernel singletons (`LiteBoxX`+`GlobalState`, a handful of allocations
/// total, each at most low-KB). 64 MiB is generous headroom for that bounded set while remaining
/// nowhere near open-ended -- the exact failure mode this shrink exists to prevent recurring.
const SHARED_KERNEL_HEAP_SIZE: usize = 64 * 1024 * 1024;

const SHARED_KERNEL_HEAP_STATE_UNINIT: u8 = 0;
const SHARED_KERNEL_HEAP_STATE_INITIALIZING: u8 = 1;
const SHARED_KERNEL_HEAP_STATE_READY: u8 = 2;

/// `SHARED_KERNEL_HEAP_STATE_UNINIT` / `_INITIALIZING` / `_READY`, see [`init_shared_kernel_heap`].
static SHARED_KERNEL_HEAP_STATE: core::sync::atomic::AtomicU8 =
    core::sync::atomic::AtomicU8::new(SHARED_KERNEL_HEAP_STATE_UNINIT);

/// Byte offset, within the shared kernel heap mapping itself, of the bump-allocation cursor
/// (Track B step 5, ADVISORY-002 3.3). **Deliberately NOT a process-local `static`** -- that was
/// the root cause of the `LITEBOX_DIAG_SHARED_HEAP_INHERIT=1` parent-crash regression documented
/// in AGENTS.md's "Real cross-process content sharing" section: with the cursor per-process, a
/// cross-process-fork CHILD that mapped the PARENT's own section still bump-allocated from an
/// independent starting point through the SAME physical pages the parent's live heap objects
/// already occupied, corrupting the parent. Putting the cursor itself inside the shared section,
/// at this fixed offset, means every process in the fork family -- creator and every inheriting
/// child alike -- reads and advances the exact SAME memory location, so a single atomic
/// read-modify-write is sufficient to make allocation itself single-writer-safe across processes.
///
/// `core::sync::atomic::AtomicUsize::compare_exchange`/`fetch_add` compile to `lock cmpxchg`/`lock
/// xadd` on x86-64 -- CPU cache-coherency-protocol instructions, not OS constructs. They are
/// correctly atomic against any physical memory two cores can see through cache coherency,
/// including the pages backing a cross-process shared section, with zero OS involvement -- the
/// same principle POSIX/SysV shared-memory IPC and libraries like Boost.Interprocess rely on for
/// lock-free shared-memory atomics on every platform. This is a fundamentally different primitive
/// from `WaitOnAddress`/keyed events (see the "hard platform constraint" note elsewhere in this
/// file): those fail cross-process because they key a WAITER by an OS-tracked identity, not
/// because CPU atomic instructions are themselves process-scoped. A raw `InterlockedCompareExchange`
/// FFI call would compile to the identical instruction on this target, so there is no correctness
/// reason to bypass `core::sync::atomic` here.
const SHARED_KERNEL_HEAP_CURSOR_OFFSET: usize = 0;

/// First byte of actual bump-allocatable space: one page after the section's base, that first
/// page reserved for the cursor at [`SHARED_KERNEL_HEAP_CURSOR_OFFSET`]. Committed eagerly by
/// whichever process FIRST creates the section (see `init_shared_kernel_heap`), since the cursor
/// must be writable before the very first real allocation that would otherwise commit its own
/// range lazily. Windows shares commit state across every view of the same pagefile-backed
/// section (already relied on, and live-verified, by `shared_kernel_heap_probe_child_read`'s
/// cross-process read of a page the PARENT alone committed), so an inheriting child never needs
/// to commit this page itself.
const SHARED_KERNEL_HEAP_DATA_OFFSET: usize = 0x1000;

/// Returns a reference to the cross-process bump-allocation cursor living AT a fixed offset
/// inside THIS process's own mapping of the shared kernel heap section (see
/// [`SHARED_KERNEL_HEAP_CURSOR_OFFSET`]). Every process in a fork family computes the identical
/// address here, because [`SHARED_KERNEL_HEAP_ACTUAL_BASE`] is only ever set to an address a real
/// mapping landed at -- for an inheriting child, `init_shared_kernel_heap` only accepts the
/// mapping at all when `landed == base` (the parent's own landing address), so this pointer names
/// the same physical page in every process in the family, never a look-alike private copy.
///
/// # Panics / safety
/// Must only be called once [`SHARED_KERNEL_HEAP_STATE`] is `_READY` (guaranteed by every caller
/// in this file, which all call [`init_shared_kernel_heap`] first) -- until then
/// `SHARED_KERNEL_HEAP_ACTUAL_BASE` is `0` and the metadata page may not be committed yet.
fn shared_heap_cursor() -> &'static core::sync::atomic::AtomicUsize {
    let base = SHARED_KERNEL_HEAP_ACTUAL_BASE.load(Ordering::Acquire);
    debug_assert_ne!(base, 0, "shared_heap_cursor() called before heap init");
    // SAFETY: see function doc comment -- `base + SHARED_KERNEL_HEAP_CURSOR_OFFSET` names a
    // committed, `AtomicUsize`-sized, naturally-aligned range inside this process's own live
    // mapping of the shared section, backed by the SAME physical pages in every process sharing
    // this section.
    unsafe {
        &*((base + SHARED_KERNEL_HEAP_CURSOR_OFFSET) as *const core::sync::atomic::AtomicUsize)
    }
}

/// The address this process's mapping actually landed at -- normally [`SHARED_KERNEL_HEAP_BASE`],
/// but see [`init_shared_kernel_heap`]'s fallback: `SHARED_KERNEL_HEAP_BASE` is a high, sparse
/// address with no OS guarantee of staying collision-free (confirmed live 2026-09-17 via `cdb`:
/// `MapViewOfFile3` at that exact address failed `STATUS_CONFLICTING_ADDRESSES` because a loaded
/// module landed inside the requested 8 GiB window, ASLR-dependent and not litebox's to control).
/// `WindowsUserland::alloc` reads this, not the constant, for its cursor/exhaustion math, so a
/// fallback landing still works correctly. True cross-process address-identical mapping (needed
/// for a future step 4's real content sharing) only holds when this equals
/// `SHARED_KERNEL_HEAP_BASE`; a process running on the fallback path is heap-functional but not
/// address-consistent with siblings that landed at the fixed base.
static SHARED_KERNEL_HEAP_ACTUAL_BASE: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// The host address range this process's own mapping of the shared kernel heap section
/// currently occupies, or `None` before [`init_shared_kernel_heap`] has run.
///
/// Root-caused 44th-pass-follow-up (2026-09-22): when [`init_shared_kernel_heap`]'s fixed
/// placement at [`SHARED_KERNEL_HEAP_BASE`] fails and it falls back to an OS-chosen address (see
/// that function's own `landed != SHARED_KERNEL_HEAP_BASE` branch), the resulting view is a
/// `SEC_RESERVE` section -- real Windows state `MEM_RESERVE`, never `MEM_COMMIT` -- landing
/// (confirmed live, `.wfgy/de_only_pass44_run2.log`) in the SAME general high-canonical-address
/// band (`0x7fef...`) `get_unmmaped_area`'s own top-down guest placement walk uses. Nothing
/// previously made this range visible to `allocate_pages`'s collision checks: `has_committed_page`
/// only flags `MEM_COMMIT` (not `MEM_RESERVE`), and the range was never registered in
/// `CLAIMED_RANGES` via `claim_range`, so a `Hint`-mode guest allocation landing exactly on it hit
/// neither guard, fell through to `allocate_pages`'s `MEM_RESERVE` branch ("the region is already
/// reserved, we just need to commit it" -- an assumption only true for litebox's OWN prior
/// `reserve_and_commit` reservations), and silently committed guest pages directly into the shared
/// arena's own section reservation. A later guest `munmap`/`mprotect` on that same range then
/// queries real Windows state that no longer matches either side's bookkeeping (observed:
/// `mbi_state=MEM_FREE`), and `process_memory_range_by_regions`'s own `VirtualFree`/`VirtualProtect`
/// call fails (`ERROR_INVALID_ADDRESS`, 487) -- surfacing as the `lib.rs:7396` panic. This helper
/// closes the visibility gap at its source: every `allocate_pages` collision check below now also
/// treats this range as permanently foreign, regardless of `MEM_COMMIT` vs `MEM_RESERVE`.
fn shared_kernel_heap_occupied_range() -> Option<core::ops::Range<usize>> {
    let base = SHARED_KERNEL_HEAP_ACTUAL_BASE.load(Ordering::Acquire);
    if base == 0 {
        None
    } else {
        Some(base..base + SHARED_KERNEL_HEAP_SIZE)
    }
}

/// True if `range` overlaps this process's own live mapping of the shared kernel heap section --
/// see [`shared_kernel_heap_occupied_range`] for why this must be checked independently of
/// `has_committed_page`/`find_foreign_claim`.
fn overlaps_shared_kernel_heap(range: &core::ops::Range<usize>) -> bool {
    shared_kernel_heap_occupied_range().is_some_and(|h| h.start < range.end && h.end > range.start)
}

/// Raw Win32 `HANDLE` value (as `usize`) of the section backing THIS process's shared-kernel-heap
/// view -- either the section [`init_shared_kernel_heap`] created itself, or one inherited from a
/// cross-process-fork PARENT (see [`shared_kernel_heap_export_for_fork_child`] and
/// [`FORK_CHILD_SHARED_HEAP_SECTION_ENV_VAR`]). Recorded so that if THIS process later becomes a
/// fork PARENT itself, it can hand the very same underlying section on to its own children --
/// content-sharing composes transitively across nested forks this way, rather than resetting to a
/// fresh private section at every fork level. `0` means "not yet initialized".
static SHARED_KERNEL_HEAP_SECTION_HANDLE: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Env var a cross-process-fork PARENT sets (via [`shared_kernel_heap_export_for_fork_child`],
/// consumed by `process_fork::spawn_process_fork_child`) carrying the DECIMAL value of its own
/// shared-kernel-heap section `HANDLE`. Never read via `std::env::var` (would allocate, recursing
/// into this very allocator on the child's first host allocation) -- see
/// [`raw_env_read_usize`]. Relies on Windows handle INHERITANCE (`CreateProcessW`'s
/// `bInheritHandles=TRUE` plus the handle itself marked inheritable) to make this same numeric
/// value valid, as a handle to the SAME kernel section object, in the child's own handle table --
/// no `DuplicateHandle`/pid-discovery round-trip is needed the way the presenter's scanout
/// handshake (`control_server.rs`) needs one, because this parent already calls `CreateProcessW`
/// for the child directly.
pub(crate) const FORK_CHILD_SHARED_HEAP_SECTION_ENV_VAR: &str =
    "LITEBOX_INTERNAL_FORK_CHILD_SHARED_HEAP_SECTION";

/// Env var carrying the DECIMAL virtual address the PARENT's shared-kernel-heap view actually
/// landed at ([`SHARED_KERNEL_HEAP_ACTUAL_BASE`]) -- the child must `MapViewOfFile3` the inherited
/// section at this EXACT address (not necessarily [`SHARED_KERNEL_HEAP_BASE`] itself, if the
/// parent hit the fixed-address collision fallback) for the two processes' pointers into this
/// region to mean the same thing. Sibling of
/// [`FORK_CHILD_SHARED_HEAP_SECTION_ENV_VAR`], read the same allocation-free way.
pub(crate) const FORK_CHILD_SHARED_HEAP_BASE_ENV_VAR: &str =
    "LITEBOX_INTERNAL_FORK_CHILD_SHARED_HEAP_BASE";

/// Sentinel value [`shared_kernel_heap_probe_parent_write`]/[`shared_kernel_heap_probe_child_read`]
/// use for the `LITEBOX_DIAG_SHARED_HEAP_PROBE=1` live cross-process content-sharing proof: the
/// parent XORs this with its own pid and writes it near the far end of the shared region; the
/// child reads the same offset back. A match (visible in the child's own inherited-stdio log line,
/// since `spawn_process_fork_child` wires the child's stderr into the parent's) is the most direct
/// possible evidence the two processes are looking at the SAME physical pages through this
/// section, not two independent copies.
const SHARED_HEAP_PROBE_MAGIC: usize = 0xC0FF_EE00_DEAD_BEEF_u64 as usize;

/// Reserves and maps the fixed-base shared kernel heap on this process's first host allocation.
///
/// Allocation-free and reentrancy-safe by construction (raw atomics only, no `OnceLock`/`Mutex`):
/// this can run on the very first allocation the process ever makes, before `Platform::new()` or
/// any allocating synchronization primitive is safe to touch -- the same constraint
/// [`DIAG_ALLOC_ENABLED_CACHE`]'s doc comment documents for this same code path (`OnceLock`'s
/// internal synchronization reentering this allocator from inside `alloc` itself self-deadlocks).
/// A losing thread spins on [`SHARED_KERNEL_HEAP_STATE`] rather than blocking, for the same
/// reason.
fn init_shared_kernel_heap() {
    loop {
        match SHARED_KERNEL_HEAP_STATE.compare_exchange(
            SHARED_KERNEL_HEAP_STATE_UNINIT,
            SHARED_KERNEL_HEAP_STATE_INITIALIZING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(SHARED_KERNEL_HEAP_STATE_READY) => return,
            Err(_) => {
                core::hint::spin_loop();
            }
        }
    }

    // Track B step 4 (ADVISORY-002 3.3): if a PARENT process exported an inheritable section
    // handle to THIS process (see `shared_kernel_heap_export_for_fork_child` and
    // `process_fork::spawn_process_fork_child`'s call site), map that SAME section at the SAME
    // address instead of reserving a private, content-independent one -- this is the actual
    // cross-process CONTENT sharing step that was previously entirely unimplemented (confirmed by
    // reading every caller as of `0ced320`: a section handle was never duplicated to a child, so
    // every process's fixed-base mapping was address-consistent but privately backed). Read via
    // the same allocation-free raw Win32 mechanism `diag_alloc_enabled` uses, since this can run
    // on the process's very first host allocation, before `std::env::var` is safe to call.
    if let (Some(handle_val), Some(base)) = (
        raw_env_read_usize(b"LITEBOX_INTERNAL_FORK_CHILD_SHARED_HEAP_SECTION\0"),
        raw_env_read_usize(b"LITEBOX_INTERNAL_FORK_CHILD_SHARED_HEAP_BASE\0"),
    ) {
        let inherited_section = handle_val as Win32_Foundation::HANDLE;
        // SAFETY: `inherited_section` is, if the env vars above were genuinely set by a real
        // parent `spawn_process_fork_child` call, a handle this process inherited at
        // `CreateProcessW` time (Windows handle inheritance preserves the numeric value, which is
        // exactly why `handle_val` -- read from an env var the PARENT populated with ITS OWN
        // value -- is already correct here with no `DuplicateHandle` step). A stale/bogus value
        // (e.g. a manually-set env var, or a value from a process this one is not actually a
        // fork child of) simply fails `MapViewOfFile3` below, handled the same as any other
        // mapping failure -- never trusted blindly.
        let view = unsafe {
            MapViewOfFile3(
                inherited_section,
                GetCurrentProcess(),
                base as *const c_void,
                0,
                SHARED_KERNEL_HEAP_SIZE,
                0,
                Win32_Memory::PAGE_READWRITE,
                core::ptr::null_mut(),
                0,
            )
        };
        let landed = view.Value as usize;
        if !view.Value.is_null() && landed == base {
            diag_raw_print(
                b"[shared_kernel_heap] INHERITED section mapped at parent's address=0x",
                landed,
                b" handle=0x",
                handle_val,
            );
            SHARED_KERNEL_HEAP_SECTION_HANDLE.store(handle_val, Ordering::Release);
            SHARED_KERNEL_HEAP_ACTUAL_BASE.store(landed, Ordering::Release);
            // Deliberately NOT initializing the cursor here (unlike the fresh-section-creation
            // path below): the cursor at `SHARED_KERNEL_HEAP_CURSOR_OFFSET` lives INSIDE this
            // inherited section's own shared pages, already initialized (and quite possibly
            // already advanced past the base by real live allocations) by whichever process
            // first created it -- see `SHARED_KERNEL_HEAP_CURSOR_OFFSET`'s doc comment. Resetting
            // it to `landed` here is exactly the previous parent-crashing bug: it would rewind a
            // live shared cursor back to the base, handing out addresses the creator's own heap
            // objects already occupy.
            SHARED_KERNEL_HEAP_STATE.store(SHARED_KERNEL_HEAP_STATE_READY, Ordering::Release);
            SHARED_KERNEL_HEAP_INHERITED_CHILD.store(true, Ordering::Release);
            shared_kernel_heap_probe_child_read(landed, true);
            return;
        }
        diag_raw_print(
            b"[shared_kernel_heap] WARN inherited-section MapViewOfFile3 FAILED wanted_base=0x",
            base,
            b" landed=0x",
            landed,
        );
        diag_raw_print(
            b"[shared_kernel_heap] WARN inherited-section win32_err=0x",
            unsafe { GetLastError() } as usize,
            b" falling back to a private section (heap-functional, not content-shared), requested_size=0x",
            SHARED_KERNEL_HEAP_SIZE,
        );
        // Falls through to the normal, private-section creation path below -- matches the
        // existing fixed-address-collision fallback philosophy (never abort over a lost
        // cross-process property when a functional, if non-shared, heap is still available).
    }

    // Live-corrected 2026-09-17 (PRD `shared-kernel-heap-eager-full-commit-not-lazy-reserve`).
    //
    // Live-eliminated alternatives, in order, all confirmed via real syscalls at this EXACT fixed
    // address before landing on the fix below -- recorded so this isn't re-attempted:
    //  1. `CreateFileMappingW(..., PAGE_READWRITE | SEC_RESERVE, ...)` + plain
    //     `MapViewOfFile3(..., allocationtype=0, PAGE_READWRITE, ...)`: fails
    //     `ERROR_INVALID_ADDRESS` (`0x1e7`). A `SEC_RESERVE` section's view cannot be mapped
    //     directly onto a chosen fixed address this way.
    //  2. Same section, `allocationtype=MEM_RESERVE`: fails `ERROR_INVALID_PARAMETER` (`0x57`).
    //  3. The documented placeholder pair -- `VirtualAlloc2(MEM_RESERVE|MEM_RESERVE_PLACEHOLDER)`
    //     then `MapViewOfFile3(..., MEM_REPLACE_PLACEHOLDER, ...)`, tried both with a bare fixed
    //     `BaseAddress` and with a `MEM_ADDRESS_REQUIREMENTS`-hinted `BaseAddress = NULL`: both
    //     fail `ERROR_INVALID_ADDRESS`.
    //  4. Plain `VirtualAlloc2(MEM_RESERVE)` with NO section at all, again both as a bare fixed
    //     address and as an address-requirements hint: both fail `ERROR_INVALID_ADDRESS` --
    //     reproduced even from a wholly unrelated PowerShell process at this exact address, so this
    //     specific 8 GiB window (`SHARED_KERNEL_HEAP_BASE`, ~8 TiB, comfortably below the ~128 TiB
    //     high-entropy-VA ceiling and not observed colliding with any other host allocation across
    //     many live runs) is one where Windows accepts a SECTION-VIEW mapping at an exact address
    //     but refuses a plain private `VirtualAlloc`-family reservation at that same exact address
    //     -- a genuine, OS-level asymmetry between the two allocation kinds, not an
    //     application-level collision (confirmed process-independent).
    //
    // Given (4), any lazy-reservation design for this address MUST go through the section-view
    // mechanism, which only succeeds with the section's ORIGINAL implicit `SEC_COMMIT` (the exact
    // call shape already single-process-verified live, many times, at this fixed address). The fix
    // is therefore not "reserve without ever committing" but "commit once, immediately release that
    // commit charge": create the section and map its view exactly as originally (full eager
    // `SEC_COMMIT`, landing at the fixed address), then IMMEDIATELY `VirtualFree(..., MEM_DECOMMIT)`
    // the entire freshly-mapped view before anything else in the process can touch it (no other
    // thread can reach this heap yet -- `SHARED_KERNEL_HEAP_STATE` is still `_INITIALIZING`, and
    // every other thread spins on it above). `VirtualFree(MEM_DECOMMIT)` on a mapped view's pages
    // is a documented, ordinary operation (the same technique large sparse memory-mapped heaps use
    // elsewhere): it releases the commit charge for those pages and returns them to "reserved,
    // uncommitted" while leaving the view's address reservation itself intact. `WindowsUserland::
    // alloc` below then re-commits each bump-allocated sub-range on demand via
    // `VirtualAlloc2(..., MEM_COMMIT, ...)`, the identical idiom this file's own
    // `was_mapped_view`/`reserve_and_commit` paths already use for committing in place over an
    // already-reserved mapped-view range -- so the FULL 8 GiB commit charge is held for a single,
    // sub-millisecond window per process (during which no other thread in this process can
    // allocate) instead of for that process's entire lifetime, which is what let N cross-process
    // fork children multiply it into real commit-limit exhaustion under real fork density.
    let size_u64 = SHARED_KERNEL_HEAP_SIZE as u64;
    // Intentional truncation: `CreateFileMappingW` takes the 64-bit size split into high/low
    // 32-bit halves, not a single 64-bit parameter (same pattern as `create_shared_memory`).
    #[expect(clippy::cast_possible_truncation)]
    let size_high = (size_u64 >> 32) as u32;
    #[expect(clippy::cast_possible_truncation)]
    let size_low = size_u64 as u32;
    // BOUNDED RETRY (kept from the original design, `e8e1ad4`): a real cross-process-fork child is
    // a genuinely separate Windows process that ALSO calls this same function on its own first
    // allocation, while a sibling process's own section from its own call may still be live --
    // live-reproduced this exact call failing transiently under real contention. Retry a bounded
    // number of times with a short backoff before giving up (this project's own "bounded retry,
    // then surface" standard) rather than a single transient failure `abort()`ing the whole fork
    // child outright (guest-visible as an unexplained `Killed`).
    const MAX_ATTEMPTS: u32 = 8;
    let mut section = core::ptr::null_mut();
    let mut last_err: u32 = 0;
    for attempt in 0..MAX_ATTEMPTS {
        section = unsafe {
            CreateFileMappingW(
                Win32_Foundation::INVALID_HANDLE_VALUE,
                core::ptr::null(),
                // `SEC_RESERVE`: reserve the 8 GiB address range with NO commit charge, instead
                // of the implicit `SEC_COMMIT` default that charged the full size against system
                // commit limit at creation time (see `SHARED_KERNEL_HEAP_SIZE`'s doc comment).
                // Live-reproduced this session: a `SEC_RESERVE` section's view failing to map at
                // the EXACT fixed address was a red herring caused by that exact address's own
                // `STATUS_CONFLICTING_ADDRESSES` collision (see the `MapViewOfFile3` fallback
                // below), not an incompatibility between `SEC_RESERVE` and `MapViewOfFile3` --
                // confirmed once the fallback (OS-chosen address) path was added and a plain
                // `SEC_COMMIT` view's `VirtualFree(MEM_DECOMMIT)` was then found to fail with
                // `ERROR_INVALID_PARAMETER` on a mapped section view regardless of address
                // (`VirtualFree` does not support decommitting a section view's pages at all --
                // only private `VirtualAlloc`-family memory). `SEC_RESERVE` avoids needing that
                // decommit step in the first place.
                Win32_Memory::PAGE_READWRITE | Win32_Memory::SEC_RESERVE,
                size_high,
                size_low,
                core::ptr::null(),
            )
        };
        if !section.is_null() {
            break;
        }
        last_err = unsafe { GetLastError() };
        if attempt + 1 < MAX_ATTEMPTS {
            // Allocation-free sleep (a raw syscall wrapper, not a libstd `Duration`-formatting
            // path) -- safe under the same reentrancy constraint as `diag_raw_print`/
            // `std::process::abort()` below. Linear backoff, 10ms/attempt (10ms..=70ms): this is
            // the process's own startup path, so it trades a bounded, sub-second worst-case delay
            // (<=280ms total) for surviving a transient condition that resolved itself in every
            // live repro within a handful of milliseconds.
            unsafe {
                windows_sys::Win32::System::Threading::Sleep(10 * (attempt + 1));
            }
        }
    }
    if section.is_null() {
        // Allocation-free failure reporting, deliberately NOT `assert!`/`panic!` with formatted
        // arguments: this can run on the process's FIRST EVER host allocation, from inside
        // `WindowsUserland::alloc` itself (`SLAB_ALLOC` is `#[global_allocator]`). `panic!`'s
        // message formatting can recurse into this very allocator (see `diag_alloc_enabled`'s
        // doc comment for the identical hazard on the same code path) -- and because
        // `SHARED_KERNEL_HEAP_STATE` is still `_INITIALIZING` (not yet `_READY`), that reentrant
        // `alloc()` call would spin forever in `init_shared_kernel_heap`'s own CAS loop instead of
        // ever reporting the real error. `diag_raw_print` + `std::process::abort()` are both
        // allocation-free and non-unwinding, so neither can recurse here.
        diag_raw_print(
            b"[shared_kernel_heap] FATAL CreateFileMappingW failed after retries win32_err=0x",
            last_err as usize,
            b" requested_size=0x",
            SHARED_KERNEL_HEAP_SIZE,
        );
        std::process::abort();
    }

    // Force EXACT placement by passing the fixed address directly as `MapViewOfFile3`'s
    // `BaseAddress` parameter, with NO `MEM_ADDRESS_REQUIREMENTS` extended parameter -- see this
    // function's own doc-comment history above for why (a non-null `BaseAddress` alone already
    // gives the "lands exactly there or fails" guarantee this needs; combining it with either an
    // address-requirements extended parameter or a `SEC_RESERVE` section fails outright).
    let mut view = unsafe {
        MapViewOfFile3(
            section,
            GetCurrentProcess(),
            SHARED_KERNEL_HEAP_BASE as *const c_void,
            0,
            SHARED_KERNEL_HEAP_SIZE,
            0,
            Win32_Memory::PAGE_READWRITE,
            core::ptr::null_mut(),
            0,
        )
    };
    let mut landed = view.Value as usize;
    if view.Value.is_null() || landed != SHARED_KERNEL_HEAP_BASE {
        // FALLBACK (found+fixed live 2026-09-17): `SHARED_KERNEL_HEAP_BASE`'s own doc comment
        // already disclosed this is "NOT a proven collision-free band" -- confirmed live via `cdb`
        // on this exact host/session: `MapViewOfFile3` at the exact fixed address fails
        // `STATUS_CONFLICTING_ADDRESSES` (surfaced as Win32 `ERROR_INVALID_ADDRESS`) because some
        // loaded module/mapping lands inside the requested 8 GiB window -- ASLR-dependent, not
        // litebox's to control, and reproduced on stock (pre-this-pass) code too, so this is a
        // pre-existing platform fragility, not a regression from today's fix. Losing exact
        // placement forfeits ONLY the not-yet-implemented step 4 cross-process address-identical
        // sharing (see `SHARED_KERNEL_HEAP_ACTUAL_BASE`'s doc comment) -- today's heap is
        // single-process-functional either way, so abort()ing the entire process over a property
        // nothing yet depends on is strictly worse than falling back to wherever the OS can
        // actually place it. Retry with `BaseAddress = NULL` (OS chooses) before giving up for
        // real.
        diag_raw_print(
            b"[shared_kernel_heap] WARN MapViewOfFile3 exact placement failed wanted=0x",
            SHARED_KERNEL_HEAP_BASE,
            b" landed=0x",
            landed,
        );
        diag_raw_print(
            b"[shared_kernel_heap] WARN MapViewOfFile3 exact placement win32_err=0x",
            unsafe { GetLastError() } as usize,
            b" falling back to OS-chosen address, requested_size=0x",
            SHARED_KERNEL_HEAP_SIZE,
        );
        view = unsafe {
            MapViewOfFile3(
                section,
                GetCurrentProcess(),
                core::ptr::null(),
                0,
                SHARED_KERNEL_HEAP_SIZE,
                0,
                Win32_Memory::PAGE_READWRITE,
                core::ptr::null_mut(),
                0,
            )
        };
        landed = view.Value as usize;
        if view.Value.is_null() {
            // Same allocation-free-failure-path constraint as the `CreateFileMappingW` check
            // above -- see that branch's comment.
            diag_raw_print(
                b"[shared_kernel_heap] FATAL MapViewOfFile3 fallback (OS-chosen address) failed win32_err=0x",
                unsafe { GetLastError() } as usize,
                b" requested_size=0x",
                SHARED_KERNEL_HEAP_SIZE,
            );
            std::process::abort();
        }
    }
    // The section handle is never closed: the view keeps the section alive for the process's
    // entire lifetime (mirrors `create_shared_memory`'s guest-facing handles, which the guest
    // owns for as long as it holds the mapping), and this mapping never goes away.
    //
    // No decommit step needed here: the section itself is `SEC_RESERVE` (see the
    // `CreateFileMappingW` call above), so this view is already reserved-not-committed the moment
    // it's mapped -- unlike a first attempt at this fix, which mapped a plain `SEC_COMMIT` view and
    // tried to `VirtualFree(..., MEM_DECOMMIT)` it back afterward. That failed live with
    // `ERROR_INVALID_PARAMETER`: `VirtualFree` does not support decommitting a mapped section
    // view's pages at all (only private `VirtualAlloc`-family memory), so `SEC_RESERVE` is not
    // just cleaner but the only one of the two that actually avoids eager commit.
    // `WindowsUserland::alloc` below commits each sub-range on demand via
    // `VirtualAlloc2(..., MEM_COMMIT, ...)` as it's actually bump-allocated.

    // Recorded (not just left as a local) so this process can hand the SAME section on to its
    // OWN fork children later -- see `SHARED_KERNEL_HEAP_SECTION_HANDLE`'s doc comment.
    SHARED_KERNEL_HEAP_SECTION_HANDLE.store(section as usize, Ordering::Release);
    SHARED_KERNEL_HEAP_ACTUAL_BASE.store(landed, Ordering::Release);

    // This process is the FIRST to create this section (never true for an inherited child, which
    // returned earlier above), so it alone is responsible for committing and initializing the
    // cursor page at `SHARED_KERNEL_HEAP_CURSOR_OFFSET` before ANY real allocation (including this
    // very function's own caller) can touch it. Committed eagerly, unlike the rest of the heap's
    // on-demand commit, precisely because the cursor itself must be writable before the first
    // lazy-commit allocation that would otherwise depend on reading it.
    //
    // SAFETY: `landed` is this process's own just-mapped, page-aligned view base; one page (0x1000
    // bytes) is well within `SHARED_KERNEL_HEAP_SIZE`, and nothing else in the process can reach
    // this heap yet (`SHARED_KERNEL_HEAP_STATE` is still `_INITIALIZING`, every other thread spins
    // above).
    let cursor_page_committed = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            landed as *mut c_void,
            SHARED_KERNEL_HEAP_DATA_OFFSET,
            Win32_Memory::MEM_COMMIT,
            Win32_Memory::PAGE_READWRITE,
            core::ptr::null_mut(),
            0,
        )
    };
    if cursor_page_committed.is_null() {
        // Same allocation-free-failure-path constraint as every other FATAL branch in this
        // function -- see the `CreateFileMappingW` failure comment above.
        diag_raw_print(
            b"[shared_kernel_heap] FATAL VirtualAlloc2(MEM_COMMIT) on cursor page failed win32_err=0x",
            unsafe { GetLastError() } as usize,
            b" addr=0x",
            landed,
        );
        std::process::abort();
    }
    // SAFETY: the cursor page was just committed above, is page-aligned, and no other thread in
    // this brand-new section can be reading/writing it yet.
    unsafe {
        (*((landed + SHARED_KERNEL_HEAP_CURSOR_OFFSET) as *const core::sync::atomic::AtomicUsize))
            .store(landed + SHARED_KERNEL_HEAP_DATA_OFFSET, Ordering::Release);
    }
    SHARED_KERNEL_HEAP_STATE.store(SHARED_KERNEL_HEAP_STATE_READY, Ordering::Release);
}

/// Ensures this process's shared kernel heap is initialized (idempotent and reentrancy-safe, see
/// [`init_shared_kernel_heap`]) and returns `(section_handle, actual_base)` for handing to a
/// cross-process-fork CHILD -- either the section this process created itself, or one it already
/// inherited from ITS OWN parent (real content-sharing composes transitively across nested forks
/// this way). Marks the handle inheritable (idempotent to call more than once across many
/// children) so a subsequent `CreateProcessW(bInheritHandles=TRUE)` actually carries it into the
/// child's handle table at the SAME numeric value. Returns `None` if the heap could not be
/// initialized at all (should not happen in practice -- this process has certainly already made
/// at least one host allocation by the time it is old enough to `fork()`) or the handle could not
/// be marked inheritable, in which case the caller falls back to letting the child create its own
/// private section, exactly as it did before this pass.
pub(crate) fn shared_kernel_heap_export_for_fork_child() -> Option<(usize, usize)> {
    if SHARED_KERNEL_HEAP_STATE.load(Ordering::Acquire) != SHARED_KERNEL_HEAP_STATE_READY {
        init_shared_kernel_heap();
    }
    if SHARED_KERNEL_HEAP_STATE.load(Ordering::Acquire) != SHARED_KERNEL_HEAP_STATE_READY {
        return None;
    }
    let handle_val = SHARED_KERNEL_HEAP_SECTION_HANDLE.load(Ordering::Acquire);
    let base = SHARED_KERNEL_HEAP_ACTUAL_BASE.load(Ordering::Acquire);
    if handle_val == 0 {
        return None;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;
    // SAFETY: `handle_val` is this process's own live section handle (either just created or
    // already inherited from an earlier fork), never closed for the process's lifetime -- see
    // `init_shared_kernel_heap`'s own "the section handle is never closed" comment.
    let ok = unsafe {
        windows_sys::Win32::Foundation::SetHandleInformation(
            handle_val as Win32_Foundation::HANDLE,
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        )
    };
    if ok == 0 {
        diag_raw_print(
            b"[shared_kernel_heap] WARN SetHandleInformation(INHERIT) failed handle=0x",
            handle_val,
            b" win32_err=0x",
            unsafe { GetLastError() } as usize,
        );
        return None;
    }
    Some((handle_val, base))
}

/// Diagnostic-only (`LITEBOX_DIAG_SHARED_HEAP_PROBE=1`), allocation-free: writes a known sentinel
/// value to a fixed offset in the LAST page of THIS process's shared-kernel-heap mapping --
/// deliberately as far as possible from anything [`WindowsUserland::alloc`]'s forward-growing bump
/// cursor could reach in a short verification run -- committing that one page first. Gives the
/// CHILD side of the same `fork()` call ([`shared_kernel_heap_probe_child_read`]) something
/// concrete to read back: the most direct possible live proof that two processes are looking at
/// the SAME physical pages through this section, not two independent copies (the exact gap this
/// pass closes -- see AGENTS.md's "the confirmed gap"). Never called except when the operator
/// opts in; a no-op otherwise, so it changes nothing about the production path's behavior or
/// memory footprint.
pub(crate) fn shared_kernel_heap_probe_parent_write(base: usize) {
    if !raw_env_is_set(b"LITEBOX_DIAG_SHARED_HEAP_PROBE\0") {
        return;
    }
    let probe_addr = base + SHARED_KERNEL_HEAP_SIZE - 0x1000;
    // SAFETY: `probe_addr` is a page-aligned address inside this process's own live
    // shared-kernel-heap reservation (`base + SIZE - 0x1000 < base + SIZE`); `VirtualAlloc2`
    // with `MEM_COMMIT` on an already-reserved section view is the same idiom
    // `WindowsUserland::alloc` itself uses for every real bump-allocated sub-range.
    let committed = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            probe_addr as *const c_void,
            0x1000,
            Win32_Memory::MEM_COMMIT,
            Win32_Memory::PAGE_READWRITE,
            core::ptr::null_mut(),
            0,
        )
    };
    if committed.is_null() {
        diag_raw_print(
            b"[shared_kernel_heap_probe] parent VirtualAlloc2(MEM_COMMIT) FAILED addr=0x",
            probe_addr,
            b" win32_err=0x",
            unsafe { GetLastError() } as usize,
        );
        return;
    }
    let pid = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessId() } as usize;
    let sentinel = SHARED_HEAP_PROBE_MAGIC ^ pid;
    // SAFETY: `probe_addr` was just committed above, is page-aligned, and this diagnostic is the
    // only writer of this specific offset anywhere in the codebase.
    unsafe {
        core::ptr::write_volatile(probe_addr as *mut usize, sentinel);
    }
    diag_raw_print(
        b"[shared_kernel_heap_probe] parent WROTE sentinel at addr=0x",
        probe_addr,
        b" value=0x",
        sentinel,
    );
}

/// Diagnostic-only (`LITEBOX_DIAG_SHARED_HEAP_PROBE=1`) counterpart to
/// [`shared_kernel_heap_probe_parent_write`], called from the CHILD side once its own shared
/// kernel heap mapping (inherited or, on the negative-control fallback path, private) is
/// established. Guards the read with `VirtualQuery` rather than reading blindly: on the private
/// (non-shared) fallback path this offset was never committed by anyone in THIS process, and a
/// raw read would fault the child's own startup -- logging a clean "not committed" line there is
/// itself a meaningful, correct diagnostic result (proof the fallback path is genuinely NOT
/// content-shared), not a condition to crash on.
pub(crate) fn shared_kernel_heap_probe_child_read(base: usize, inherited: bool) {
    if !raw_env_is_set(b"LITEBOX_DIAG_SHARED_HEAP_PROBE\0") {
        return;
    }
    let probe_addr = base + SHARED_KERNEL_HEAP_SIZE - 0x1000;
    let mut mbi = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
    let q = unsafe {
        Win32_Memory::VirtualQuery(
            probe_addr as *const c_void,
            &raw mut mbi,
            core::mem::size_of::<Win32_Memory::MEMORY_BASIC_INFORMATION>(),
        )
    };
    if q == 0 || mbi.State != Win32_Memory::MEM_COMMIT {
        diag_raw_print(
            b"[shared_kernel_heap_probe] child addr=0x",
            probe_addr,
            b" NOT COMMITTED, inherited=0x",
            usize::from(inherited),
        );
        return;
    }
    // SAFETY: `VirtualQuery` just confirmed this page is `MEM_COMMIT`.
    let observed = unsafe { core::ptr::read_volatile(probe_addr as *const usize) };
    diag_raw_print(
        b"[shared_kernel_heap_probe] child OBSERVED at addr=0x",
        probe_addr,
        b" value=0x",
        observed,
    );
}

/// Standalone, bounded bump arena over the fixed-base shared kernel section -- **deliberately NOT
/// wired to `GlobalAlloc`/`SLAB_ALLOC`** (see [`SLAB_ALLOC`]'s doc comment for why routing every
/// host-heap allocation through here was reverted 2026-09-17). This is the machinery a follow-up
/// session's `LiteBoxX`/`GlobalState`-only migration needs: call this directly (never via `Box`/
/// `Arc`, which always go through `#[global_allocator]`) to get a raw pointer into the shared
/// section, `ptr::write` the value into place, and wrap it in a manually-refcounted smart pointer
/// with a custom `Drop` (Rust stable has no `allocator_api`/`Box::new_in`, so `Arc<T>` itself can
/// never be told to use a non-default allocator; its `ArcInner` layout is also a private,
/// unstable implementation detail, so `Arc::from_raw` over manually-placed bytes is unsound --
/// only a hand-rolled wrapper type works here). **Not yet called by any real caller as of this
/// pass** -- `LiteBoxX`/`GlobalState` still allocate via ordinary `Arc::new` on the reverted
/// private heap; wiring them through this function requires that wrapper type, which itself needs
/// a new trait (analogous to `RawMutexProvider`) threaded through `litebox`/`litebox_shim_linux`'s
/// generic `Platform` bound with a no-op default for every non-Windows platform, since `LiteBox::
/// new`/`LinuxShimBuilder::build` are shared, platform-generic code paths. Left as real,
/// live-verifiable (see the existing `LITEBOX_DIAG_SHARED_HEAP_PROBE=1` sentinel probes, which
/// operate on raw bytes and work unchanged against this smaller region) infrastructure rather than
/// deleted, since the atomic cursor / fixed-base / cross-process handle-inheritance mechanism
/// (`3e81e1d`/`3d661d2`) is correct, live-verified, and exactly what this smaller use case needs.
pub(crate) fn shared_kernel_arena_alloc(layout: &std::alloc::Layout) -> Option<(usize, usize)> {
    let size = core::cmp::max(
        layout.size().next_power_of_two(),
        core::cmp::max(layout.align(), 0x1000) << 1,
    );

    if SHARED_KERNEL_HEAP_STATE.load(Ordering::Acquire) != SHARED_KERNEL_HEAP_STATE_READY {
        init_shared_kernel_heap();
    }

    // Bump-allocate a sub-range of the single fixed-base mapping reserved by
    // `init_shared_kernel_heap`. `size` is always a power of two and at least 4 KiB (see above),
    // so the cursor -- itself starting at the page-aligned actual base (see
    // [`SHARED_KERNEL_HEAP_ACTUAL_BASE`] -- usually `SHARED_KERNEL_HEAP_BASE`, but
    // `init_shared_kernel_heap`'s fallback may have landed elsewhere) and only ever advanced by
    // such sizes -- stays page-aligned throughout.
    let actual_base = SHARED_KERNEL_HEAP_ACTUAL_BASE.load(Ordering::Acquire);
    // Cross-process-safe cursor: lives INSIDE the shared section itself (see
    // `shared_heap_cursor`'s doc comment), not a process-local `static`, so a cross-process-fork
    // child sharing this section via inherited handle advances the exact SAME cursor the parent
    // (and every sibling) sees, via one atomic CAS loop -- correct regardless of which process
    // actually performs the allocation.
    let cursor = shared_heap_cursor();
    let mut cur = cursor.load(Ordering::Acquire);
    let addr = loop {
        let next = cur.checked_add(size)?;
        if next > actual_base + SHARED_KERNEL_HEAP_SIZE {
            // Exhausted the reservation; surfaces as an ordinary allocator OOM.
            return None;
        }
        match cursor.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break cur,
            Err(actual) => cur = actual,
        }
    };

    // On-demand commit: `init_shared_kernel_heap` leaves this whole mapping reserved-not-committed
    // (`SEC_RESERVE`), so the sub-range this call just exclusively claimed via the CAS above is
    // reserved address space only, not yet backed by memory/pagefile. Commit exactly that range
    // now, matching the codebase's established lazy-commit pattern for other large reservations
    // (this file's own `was_mapped_view`/`reserve_and_commit` paths, which likewise call
    // `VirtualAlloc2(..., MEM_COMMIT, ...)` in place over an already-reserved mapped-view range).
    // `addr`/`size` are both page-aligned (see this function's own comment above), so no rounding
    // is needed.
    // SAFETY: `addr` names a page-aligned sub-range of the shared section's live view that this
    // call just exclusively claimed via the CAS above -- no other caller can commit or touch this
    // exact range concurrently.
    let committed = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            addr as *mut c_void,
            size,
            Win32_Memory::MEM_COMMIT,
            Win32_Memory::PAGE_READWRITE,
            core::ptr::null_mut(),
            0,
        )
    };
    if committed.is_null() {
        diag_raw_print(
            b"[shared_kernel_arena] FATAL VirtualAlloc2(MEM_COMMIT) failed win32_err=0x",
            unsafe { GetLastError() } as usize,
            b" addr=0x",
            addr,
        );
        std::process::abort();
    }
    Some((addr, size))
}

/// Hand-rolled control block for [`SharedArc`], placed immediately before `T` in the same
/// [`shared_kernel_arena_alloc`] allocation. **Deliberately NOT `std::sync::Arc`'s `ArcInner`** --
/// that layout is a private, unstable std implementation detail (no `#[repr(C)]`, no stability
/// guarantee), so `Arc::from_raw` over manually-placed bytes in a shared section would rely on
/// undocumented layout and is unsound. Every field here is defined by this codebase, `#[repr(C)]`
/// for a stable cross-process layout, and touched only through `core::sync::atomic` (which compile
/// to plain `lock`-prefixed x86-64 instructions -- CPU cache-coherency primitives, not OS
/// constructs -- so they are correctly atomic across the shared section exactly as
/// [`shared_heap_cursor`]'s doc comment already establishes for the arena's own bump cursor).
///
/// **No `weak` field.** `std::sync::Arc<T>`'s split strong/weak scheme exists to let a `Weak<T>`
/// observe "has the value been dropped" without keeping it alive. Nothing in this codebase needs a
/// non-owning cross-process reference to a kernel singleton yet -- every real call site (a planned
/// future `GlobalState`/`LiteBoxX` migration) wants ordinary shared ownership, matching how
/// `Arc<T>` is already used at those sites today. Adding an unused `Weak` mechanism now would be
/// speculative generality with no caller (YAGNI); if a real need for one shows up, it is a small,
/// additive change to this struct, not a redesign.
#[repr(C)]
struct SharedArcInner<T> {
    /// Number of live [`SharedArc<T>`] handles across every process attached to this allocation.
    /// Incremented by [`SharedArc::new`]/[`SharedArc::clone`]/[`SharedArc::attach`], decremented by
    /// [`SharedArc::drop`].
    strong: core::sync::atomic::AtomicUsize,
    value: T,
}

/// Hand-rolled, cross-process-safe shared-ownership smart pointer over a value placed in the
/// fixed-base [`shared_kernel_arena_alloc`] arena (`advisor/ADVISORY-002-d-zero-fork.md` section
/// 3.3, Track B step 3's final piece). Ergonomically mirrors `std::sync::Arc<T>` (`Clone`, `Drop`,
/// `Deref`) as closely as possible to minimize churn at the call sites a future pass migrates onto
/// it (`LiteBoxX`/`GlobalState`, both currently plain `Arc::new(...)`) -- but see the "No real
/// deallocation" note on [`SharedArc::drop`] below for the one place this type's semantics
/// deliberately diverge from `Arc<T>`'s.
///
/// # Why not `std::sync::Arc`
/// See [`SharedArcInner`]'s doc comment: `Arc::from_raw` over manually-placed bytes relies on an
/// unstable, private layout and is unsound. Stable Rust also has no `allocator_api`/`Box::new_in`,
/// so `Arc::new_in` is not an option either -- a hand-rolled control block is the only sound
/// mechanism.
///
/// # Cross-process attach protocol
/// [`SharedArc::new`] (called once, by whichever process creates the value) returns both the
/// handle and its [`SharedArc::arena_offset`] -- a BYTE OFFSET from the shared arena's own base
/// ([`SHARED_KERNEL_HEAP_ACTUAL_BASE`]), not an absolute address, because the arena's actual
/// landing address can differ per process on the fixed-address-collision fallback path (see
/// [`SHARED_KERNEL_HEAP_BASE`]'s doc comment) -- an offset stays valid in every process that DID
/// land at the true shared base, and is cheap to hand to a child the same way the section
/// handle/base pair already travel today (an env var the parent sets before `CreateProcessW`,
/// read allocation-free via [`raw_env_read_usize`]-style parsing, see
/// [`FORK_CHILD_SHARED_HEAP_SECTION_ENV_VAR`] for the existing precedent this mirrors). A child
/// that has the SAME arena section mapped at the SAME fixed address (i.e. reached
/// [`init_shared_kernel_heap`]'s inherited-section branch) then calls [`SharedArc::attach`] with
/// that plain `usize` offset to obtain its own independently-owned handle to the SAME `T`.
pub struct SharedArc<T> {
    ptr: core::ptr::NonNull<SharedArcInner<T>>,
}

// SAFETY: `SharedArc<T>` provides the same cross-thread/cross-process shared-access guarantees as
// `std::sync::Arc<T>` -- every mutation of the control block goes through `core::sync::atomic`,
// and `T` itself is required to be `Sync` (readable concurrently through `Deref`) and `Send`
// (its destructor, were one ever run, could run on a different thread/process than the one that
// last touched it) for exactly the reasons `Arc<T>`'s own `unsafe impl` requires them.
unsafe impl<T: Sync + Send> Send for SharedArc<T> {}
unsafe impl<T: Sync + Send> Sync for SharedArc<T> {}

impl<T> SharedArc<T> {
    /// Places `value` plus a fresh [`SharedArcInner`] control block (`strong = 1`) into the shared
    /// kernel arena via [`shared_kernel_arena_alloc`], and returns the new handle together with
    /// its [`arena_offset`](SharedArc::arena_offset) -- the value a caller must hand to any other
    /// process that will [`SharedArc::attach`] to this same allocation. Returns `None` on arena
    /// exhaustion (the same bounded-64-MiB-pool OOM every other `shared_kernel_arena_alloc` caller
    /// can hit), leaving nothing partially constructed.
    fn new(value: T) -> Option<(Self, usize)> {
        let layout = std::alloc::Layout::new::<SharedArcInner<T>>();
        let (addr, _size) = shared_kernel_arena_alloc(&layout)?;
        let inner_ptr = addr as *mut SharedArcInner<T>;
        // SAFETY: `shared_kernel_arena_alloc` just returned `addr` as a freshly, exclusively
        // claimed (via its internal atomic-cursor CAS) and committed sub-range of the shared
        // arena, sized and aligned for at least `layout` (`SharedArcInner<T>`'s own layout, which
        // `Layout::new` computes correctly including `T`'s alignment) -- no other code anywhere
        // can be reading or writing these bytes yet, so a raw `ptr::write` of the fully-formed
        // control block is sound and does not need to (and must not, being uninitialized memory)
        // drop any previous value.
        unsafe {
            inner_ptr.write(SharedArcInner {
                strong: core::sync::atomic::AtomicUsize::new(1),
                value,
            });
        }
        let base = SHARED_KERNEL_HEAP_ACTUAL_BASE.load(Ordering::Acquire);
        let offset = inner_ptr as usize - base;
        // SAFETY: `inner_ptr` was just derived from a non-null `addr` above (`shared_kernel_arena_alloc`
        // never returns a null address for a `Some` result -- it aborts the process instead on any
        // commit failure).
        let ptr = unsafe { core::ptr::NonNull::new_unchecked(inner_ptr) };
        Some((SharedArc { ptr }, offset))
    }

    /// Attaches to an existing [`SharedArc<T>`] allocation created by [`SharedArc::new`] in
    /// (typically) another process, given the BYTE OFFSET that process's own `arena_offset`
    /// returned. Atomically increments the shared strong count, so the returned handle is a fully
    /// independent owning reference -- dropping the ORIGINAL creator's handle first does not
    /// invalidate this one.
    ///
    /// # Safety
    /// The caller must ensure:
    /// - This process has already reached [`init_shared_kernel_heap`]'s inherited-section
    ///   success path (i.e. [`SHARED_KERNEL_HEAP_STATE`] is `_READY` AND this process mapped the
    ///   SAME underlying section at the SAME address as the creator -- never the
    ///   private-fallback path, which contains none of the creator's data).
    /// - `offset` genuinely came from a real [`SharedArc::<T>::new`] call for THIS SAME `T` (a
    ///   wrong offset, or the right offset with the wrong `T`, reads/mutates arbitrary shared
    ///   bytes as if they were a valid `SharedArcInner<T>` -- there is no tag or runtime
    ///   type-check, exactly as `Arc::from_raw` itself has none).
    /// - The allocation `offset` names has not been (and never will be) reclaimed -- always true
    ///   today, since [`shared_kernel_arena_alloc`] never reclaims (see [`SharedArc::drop`]).
    unsafe fn attach(offset: usize) -> Self {
        let base = SHARED_KERNEL_HEAP_ACTUAL_BASE.load(Ordering::Acquire);
        let inner_ptr = (base + offset) as *mut SharedArcInner<T>;
        // SAFETY: caller contract above.
        let inner = unsafe { &*inner_ptr };
        // `Relaxed` suffices for the increment itself (matches `Arc::clone`'s own reasoning: no
        // memory operation needs to happen-before this one -- the new handle cannot be used until
        // after this call returns on this same thread, which is already ordered).
        inner.strong.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `inner_ptr` is non-null (derived from a non-zero `base` plus an in-range
        // `offset`, both preconditions of this function per its own safety doc).
        let ptr = unsafe { core::ptr::NonNull::new_unchecked(inner_ptr) };
        SharedArc { ptr }
    }

    /// This handle's byte offset from the shared arena's base -- the value to hand to another
    /// process's [`SharedArc::attach`]. See [`SharedArc::new`]'s doc comment for why this is an
    /// offset, not an absolute address.
    fn arena_offset(&self) -> usize {
        let base = SHARED_KERNEL_HEAP_ACTUAL_BASE.load(Ordering::Acquire);
        self.ptr.as_ptr() as usize - base
    }

    /// Current strong count, diagnostic/verification use only (matches `Arc::strong_count`'s own
    /// "racy in the presence of concurrent clones/drops" caveat -- reading it is never itself
    /// unsound, just not linearizable with concurrent mutators).
    fn strong_count(&self) -> usize {
        // SAFETY: `self.ptr` always names a live `SharedArcInner<T>` for as long as `self` exists
        // (this handle itself holds one of the counted strong references).
        unsafe { self.ptr.as_ref() }
            .strong
            .load(Ordering::Acquire)
    }
}

impl<T> Clone for SharedArc<T> {
    fn clone(&self) -> Self {
        // SAFETY: see `strong_count`'s identical reasoning.
        unsafe { self.ptr.as_ref() }
            .strong
            .fetch_add(1, Ordering::Relaxed);
        SharedArc { ptr: self.ptr }
    }
}

impl<T> core::ops::Deref for SharedArc<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: `self.ptr` always names a live, fully-initialized `SharedArcInner<T>` for as
        // long as `self` exists.
        &unsafe { self.ptr.as_ref() }.value
    }
}

impl<T> Drop for SharedArc<T> {
    /// **No real deallocation/reclaim, by design -- confirmed, not an oversight.** Two independent
    /// reasons, both specific to this type's actual use case (long-lived kernel singletons like
    /// `GlobalState`/`LiteBoxX`, never a general-purpose dynamically-allocated object):
    ///
    /// 1. [`shared_kernel_arena_alloc`] is a pure bump allocator with no free list (see its own
    ///    doc comment) -- there is no mechanism to give bytes back to the arena even if this were
    ///    the very last handle anywhere, so "reclaim the memory" is not an available option here
    ///    regardless of refcount semantics.
    /// 2. Running `T`'s destructor from whichever process happens to observe `strong == 0` would
    ///    be actively unsound for the intended `T`s: `GlobalState`/`LiteBoxX` are expected to
    ///    embed real per-process OS resources (`HANDLE`s, fds) at some fields, and a `HANDLE`
    ///    value is only meaningful in the process that owns it -- running a `Drop` impl that
    ///    closes such a handle from an arbitrary OTHER process in the fork family would close
    ///    whatever unrelated handle number happens to be live there instead. A kernel singleton
    ///    is, by this project's own design intent (`docs/AGENTS_ARCHIVE_2026-09-17.md`'s "Shared
    ///    kernel heap" section), meant to outlive every process in the fork family for the whole
    ///    guest session -- i.e. never actually reach `strong == 0` while the guest is alive at
    ///    all -- so a correct, non-reclaiming `Drop` costs nothing in practice for this use case.
    ///
    /// The strong count is still decremented on every drop (diagnostic/verification value, and
    /// keeps [`SharedArc::strong_count`] meaningful for live cross-process proof), but reaching
    /// zero intentionally does nothing further: no destructor call, no arena reclaim. This mirrors
    /// the arena's own established "bump allocator, dead-in-practice on exit" philosophy
    /// (`WindowsUserland::free`'s doc comment) rather than building a general-purpose refcounted
    /// allocator no real caller of this type needs.
    fn drop(&mut self) {
        // SAFETY: `self.ptr` always names a live `SharedArcInner<T>` until this very call.
        unsafe { self.ptr.as_ref() }
            .strong
            .fetch_sub(1, Ordering::Release);
    }
}

/// This process's own `SharedArc::arena_offset` for [`litebox::platform::SharedKernelStateSlot::
/// LiteBoxX`]/[`ShimGlobalState`](litebox::platform::SharedKernelStateSlot::ShimGlobalState),
/// set the one time [`WindowsUserland::create_shared_kernel_state`] actually creates (never
/// attaches) an allocation for that slot -- read by `process_fork::spawn_process_fork_child` when
/// exporting this process's shared kernel state to a cross-process-fork child, the real
/// (non-diagnostic) counterpart to [`FORK_CHILD_SHARED_ARC_PROBE_OFFSET_ENV_VAR`]'s isolated
/// probe struct. `usize::MAX` means "not yet created in this process" (never a valid offset:
/// [`shared_kernel_arena_alloc`] never hands out the arena's own final byte as a fresh
/// allocation's base).
static SHARED_LITEBOXX_OFFSET: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);
static SHARED_GLOBALSTATE_OFFSET: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);

/// Set by [`init_shared_kernel_heap`]'s inherited-section success path: `true` exactly when THIS
/// process mapped the SAME shared-kernel-heap section at the SAME address as an ancestor (i.e.
/// this process is a `LITEBOX_PROCESS_FORK=1` cross-process-fork child that can genuinely reach
/// its ancestor's `SharedArc` allocations), `false` for the process that first creates the
/// section (including the root of a fork family) and for the fixed-address-collision fallback
/// path (heap-functional but NOT content-shared -- see [`SHARED_KERNEL_HEAP_ACTUAL_BASE`]'s doc
/// comment). Backs [`WindowsUserland::is_shared_kernel_state_attach_child`].
static SHARED_KERNEL_HEAP_INHERITED_CHILD: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Real (non-diagnostic) per-[`litebox::platform::SharedKernelStateSlot`] env var carrying the
/// DECIMAL [`SharedArc::arena_offset`] a cross-process-fork PARENT exports for that slot --
/// siblings of [`FORK_CHILD_SHARED_ARC_PROBE_OFFSET_ENV_VAR`], read the same allocation-free way.
pub(crate) fn shared_kernel_state_slot_env_var(
    slot: litebox::platform::SharedKernelStateSlot,
) -> &'static str {
    match slot {
        litebox::platform::SharedKernelStateSlot::LiteBoxX => {
            "LITEBOX_INTERNAL_FORK_CHILD_LITEBOXX_OFFSET"
        }
        litebox::platform::SharedKernelStateSlot::ShimGlobalState => {
            "LITEBOX_INTERNAL_FORK_CHILD_GLOBALSTATE_OFFSET"
        }
    }
}

/// This process's own `arena_offset` for `slot`, if [`WindowsUserland::create_shared_kernel_state`]
/// has created one (never true for a process that only ever ATTACHED to an ancestor's). Read by
/// `process_fork::spawn_process_fork_child` to decide what (if anything) to export to a
/// cross-process-fork child for this slot.
pub(crate) fn shared_kernel_state_offset(
    slot: litebox::platform::SharedKernelStateSlot,
) -> Option<usize> {
    let cell = match slot {
        litebox::platform::SharedKernelStateSlot::LiteBoxX => &SHARED_LITEBOXX_OFFSET,
        litebox::platform::SharedKernelStateSlot::ShimGlobalState => &SHARED_GLOBALSTATE_OFFSET,
    };
    let v = cell.load(Ordering::Acquire);
    (v != usize::MAX).then_some(v)
}

/// Real (non-diagnostic, production) counterpart to the isolated `SharedArc<SharedArcProbeData>`
/// proof above (`LITEBOX_DIAG_SHARED_ARC_PROBE=1`): backs `litebox::platform::
/// SharedKernelStateProvider` for `litebox::LiteBox`'s own `LiteBoxX` singleton and
/// `litebox_shim_linux::GlobalState`, so a `LITEBOX_PROCESS_FORK=1` cross-process fork child can
/// genuinely ATTACH to its ancestor's live instance instead of every process in the fork family
/// independently constructing its own private copy -- see that trait's own doc comment for the
/// full "consistent address, independent copy" problem this closes, and
/// `docs/AGENTS_ARCHIVE_2026-09-17.md`'s create-vs-attach section for the live cross-process
/// proof this was verified with.
impl litebox::platform::SharedKernelStateProvider for WindowsUserland {
    type Handle<T: Send + Sync + 'static> = SharedArc<T>;

    fn is_shared_kernel_state_attach_child(
        &self,
        _slot: litebox::platform::SharedKernelStateSlot,
    ) -> bool {
        // `SHARED_KERNEL_HEAP_INHERITED_CHILD` is only ever SET from inside
        // `init_shared_kernel_heap`'s inherited-section branch -- and nothing implicitly calls
        // that function anymore for an ordinary process (ordinary `GlobalAlloc` traffic no longer
        // touches it at all post-revert, see `SLAB_ALLOC`'s doc comment), so without this explicit
        // call here, THIS check would always observe the untouched, default `false` on a fresh
        // cross-process-fork child that has made no other shared-arena call yet -- silently
        // forcing every process onto the "construct fresh" branch regardless of whether it could
        // actually have attached. Idempotent and safe to call from ordinary, non-reentrant code
        // (this is a `LiteBox::new`/`LinuxShimBuilder::build`-time call, not one reachable from
        // `WindowsUserland::alloc` itself), exactly like `shared_arc_probe_child_attach`'s own
        // identical call.
        if SHARED_KERNEL_HEAP_STATE.load(Ordering::Acquire) != SHARED_KERNEL_HEAP_STATE_READY {
            init_shared_kernel_heap();
        }
        // Same boolean for every slot: a process either genuinely reached the inherited-section
        // success path (and can therefore attach to ANY slot its ancestor created) or it did not
        // (the process that creates the section in the first place, or one that fell back to a
        // private, non-content-shared section) -- see `SHARED_KERNEL_HEAP_INHERITED_CHILD`'s own
        // doc comment.
        SHARED_KERNEL_HEAP_INHERITED_CHILD.load(Ordering::Acquire)
    }

    fn create_shared_kernel_state<T: Send + Sync + 'static>(
        &self,
        slot: litebox::platform::SharedKernelStateSlot,
        value: T,
    ) -> Self::Handle<T> {
        let (arc, offset) =
            SharedArc::new(value).expect("shared_kernel_state: arena_alloc failed (arena exhausted)");
        let offset_cell = match slot {
            litebox::platform::SharedKernelStateSlot::LiteBoxX => &SHARED_LITEBOXX_OFFSET,
            litebox::platform::SharedKernelStateSlot::ShimGlobalState => &SHARED_GLOBALSTATE_OFFSET,
        };
        offset_cell.store(offset, Ordering::Release);
        arc
    }

    fn attach_shared_kernel_state<T: Send + Sync + 'static>(
        &self,
        slot: litebox::platform::SharedKernelStateSlot,
    ) -> Option<Self::Handle<T>> {
        if SHARED_KERNEL_HEAP_STATE.load(Ordering::Acquire) != SHARED_KERNEL_HEAP_STATE_READY {
            init_shared_kernel_heap();
        }
        if !SHARED_KERNEL_HEAP_INHERITED_CHILD.load(Ordering::Acquire) {
            return None;
        }
        // Allocation-free reads (see `raw_env_read_usize`'s own doc comment for why: this can run
        // before the process's own heap is otherwise usable) -- byte-string literals rather than
        // `shared_kernel_state_slot_env_var`'s `&str` (used only by the export/write side in
        // `process_fork.rs`, which already has an allocator available) to avoid building a
        // NUL-terminated buffer at runtime here.
        let offset = match slot {
            litebox::platform::SharedKernelStateSlot::LiteBoxX => {
                raw_env_read_usize(b"LITEBOX_INTERNAL_FORK_CHILD_LITEBOXX_OFFSET\0")
            }
            litebox::platform::SharedKernelStateSlot::ShimGlobalState => {
                raw_env_read_usize(b"LITEBOX_INTERNAL_FORK_CHILD_GLOBALSTATE_OFFSET\0")
            }
        }?;
        // Recorded (not just used locally) so THIS process can re-export the SAME offset to its
        // OWN fork children later, exactly like `SHARED_KERNEL_HEAP_SECTION_HANDLE`'s own
        // "content-sharing composes transitively across nested forks" property -- without this,
        // an attach-only process (never a creator) would have nothing for
        // `shared_kernel_state_offset` to return, breaking propagation past one fork generation.
        let offset_cell = match slot {
            litebox::platform::SharedKernelStateSlot::LiteBoxX => &SHARED_LITEBOXX_OFFSET,
            litebox::platform::SharedKernelStateSlot::ShimGlobalState => &SHARED_GLOBALSTATE_OFFSET,
        };
        offset_cell.store(offset, Ordering::Release);
        // SAFETY: `is_shared_kernel_state_attach_child` (checked via
        // `SHARED_KERNEL_HEAP_INHERITED_CHILD` above) confirmed this process mapped the SAME
        // shared-kernel-heap section at the SAME address as its ancestor, and `offset` was read
        // from the env var the ancestor's own `create_shared_kernel_state` populated for this
        // EXACT slot right before spawning this process (see
        // `process_fork::spawn_process_fork_child`) -- the caller's own generic `T` is the same
        // monomorphization in both processes since a cross-process-fork child re-execs the
        // identical binary.
        Some(unsafe { SharedArc::<T>::attach(offset) })
    }

    /// Real (non-default) implementation: hands back a raw pointer into the SAME fixed-base
    /// [`shared_kernel_arena_alloc`] arena `create_shared_kernel_state`/`SharedArc` themselves
    /// use. The address this returns is, by that arena's own fixed-base design (see
    /// [`SHARED_KERNEL_HEAP_ACTUAL_BASE`]'s doc comment), the SAME valid pointer value in every
    /// process that reached the inherited-section success path -- unlike `SharedArc<T>`, no
    /// offset/attach round-trip is needed for the CALLER to reuse this pointer later, because the
    /// caller embeds the pointer/slice itself directly into an already-shared struct (e.g.
    /// `litebox::net::Network::socket_set`, itself a field of the `SharedArc`-placed
    /// `litebox_shim_linux::GlobalState`) at construction time, and that struct's own bytes are
    /// what actually crosses to attaching processes.
    fn shared_kernel_arena_alloc_bytes(
        &self,
        layout: core::alloc::Layout,
    ) -> Option<core::ptr::NonNull<u8>> {
        let (addr, _size) = shared_kernel_arena_alloc(&layout)?;
        core::ptr::NonNull::new(addr as *mut u8)
    }
}

/// Isolated test payload for the live cross-process [`SharedArc<T>`] proof
/// (`LITEBOX_DIAG_SHARED_ARC_PROBE=1`) -- unrelated to any real guest-visible state, exactly
/// mirroring how [`SHARED_HEAP_PROBE_MAGIC`]'s sentinel proof is kept separate from production
/// data. `magic` proves the child observes the PARENT's `ptr::write`d bytes through the wrapper;
/// `counter` proves a child-side mutation (through `Deref`, via its own interior `AtomicUsize` --
/// `SharedArc<T>` itself only ever hands out `&T`, matching `Arc<T>`) is a write to the SAME
/// physical memory the parent can also observe, not a private copy.
#[repr(C)]
struct SharedArcProbeData {
    magic: usize,
    counter: core::sync::atomic::AtomicUsize,
}

const SHARED_ARC_PROBE_MAGIC: usize = 0x5AC5_5AC5_5AC5_5AC5;

/// Env var carrying the DECIMAL [`SharedArc::arena_offset`] of this run's probe allocation --
/// sibling of [`FORK_CHILD_SHARED_HEAP_SECTION_ENV_VAR`]/[`FORK_CHILD_SHARED_HEAP_BASE_ENV_VAR`],
/// set at the same call site in `process_fork::spawn_process_fork_child`, read the same
/// allocation-free way from [`init_shared_kernel_heap`]'s inherited-section branch.
pub(crate) const FORK_CHILD_SHARED_ARC_PROBE_OFFSET_ENV_VAR: &str =
    "LITEBOX_INTERNAL_FORK_CHILD_SHARED_ARC_PROBE_OFFSET";

/// This process's own probe handle plus one extra `Clone` of it, kept alive for the process's
/// whole lifetime so `strong_count` stays meaningful across every child this parent forks during
/// one diagnostic run (never dropped mid-run) -- created at most once (`OnceLock`), reused by
/// every subsequent fork.
static SHARED_ARC_PROBE_PARENT: OnceLock<(SharedArc<SharedArcProbeData>, SharedArc<SharedArcProbeData>)> =
    OnceLock::new();

/// Diagnostic-only (`LITEBOX_DIAG_SHARED_ARC_PROBE=1`), called from the PARENT side of
/// `process_fork::spawn_process_fork_child` right before spawning a real cross-process-fork child.
/// On this process's first call, creates the probe allocation (`strong` -> 1) and immediately
/// `Clone`s it once more (`strong` -> 2) to prove same-process `Clone` works before any
/// cross-process attach is involved; every call (first or not) prints the current strong count and
/// returns the allocation's `arena_offset` for the caller to hand to the child. Requires the
/// shared-heap-inherit gate to already be exporting a section to the child (this probe rides on
/// that same section, it does not create its own).
pub(crate) fn shared_arc_probe_parent_prepare() -> usize {
    let (first, _clone) = SHARED_ARC_PROBE_PARENT.get_or_init(|| {
        let (arc, offset) = SharedArc::new(SharedArcProbeData {
            magic: SHARED_ARC_PROBE_MAGIC,
            counter: core::sync::atomic::AtomicUsize::new(0),
        })
        .expect("shared_arc_probe: arena_alloc failed for probe struct");
        eprintln!(
            "[shared_arc_probe] parent CREATED offset=0x{offset:x} strong={}",
            arc.strong_count()
        );
        let cloned = arc.clone();
        eprintln!(
            "[shared_arc_probe] parent CLONED strong={} (expect 2)",
            cloned.strong_count()
        );
        (arc, cloned)
    });
    let offset = first.arena_offset();
    eprintln!(
        "[shared_arc_probe] parent pre-fork strong={} offset=0x{offset:x}",
        first.strong_count()
    );
    offset
}

/// Diagnostic-only (`LITEBOX_DIAG_SHARED_ARC_PROBE=1`) counterpart to
/// [`shared_arc_probe_parent_prepare`] -- `pub` (not `pub(crate)`) because the child process's own
/// startup lives in the separate `litebox_runner_linux_on_windows_userland` crate, which calls
/// this explicitly once early in a resumed cross-process-fork child's life (see that crate's
/// `diag_process_fork_task_resume_probe`, right before handing off to the real task-resume probe).
/// That explicit call is necessary, not merely a convenience: unlike the earlier `LiteBoxX`/
/// `GlobalState`-routed-through-the-shared-heap design this superseded, ordinary `GlobalAlloc`
/// traffic no longer touches [`init_shared_kernel_heap`] AT ALL post-revert (see [`SLAB_ALLOC`]'s
/// doc comment) -- so nothing implicitly initializes/inherits the shared arena in a plain
/// cross-process-fork child anymore, and this function calls [`init_shared_kernel_heap`] itself
/// (idempotent, safe to call from ordinary non-reentrant code -- the reentrancy constraint only
/// binds callers reachable from `WindowsUserland::alloc` itself) rather than relying on being
/// invoked from inside it.
pub fn shared_arc_probe_child_attach() {
    if !raw_env_is_set(b"LITEBOX_DIAG_SHARED_ARC_PROBE\0") {
        return;
    }
    init_shared_kernel_heap();
    if SHARED_KERNEL_HEAP_STATE.load(Ordering::Acquire) != SHARED_KERNEL_HEAP_STATE_READY {
        diag_raw_print(
            b"[shared_arc_probe] child: shared kernel heap not READY, skipping",
            0,
            b"",
            0,
        );
        return;
    }
    let Some(offset) = raw_env_read_usize(b"LITEBOX_INTERNAL_FORK_CHILD_SHARED_ARC_PROBE_OFFSET\0")
    else {
        diag_raw_print(
            b"[shared_arc_probe] child: no offset env var set, skipping",
            0,
            b"",
            0,
        );
        return;
    };
    // SAFETY: gated on `LITEBOX_DIAG_SHARED_ARC_PROBE` being explicitly opted into by the
    // operator running this exact diagnostic, `offset` came from this run's own
    // `shared_arc_probe_parent_prepare` (the only writer of the env var this reads), and this
    // function is only reachable from `init_shared_kernel_heap`'s inherited-section branch, which
    // already confirmed `landed == base` (this process mapped the SAME section at the SAME
    // address as the parent that created the probe).
    let attached = unsafe { SharedArc::<SharedArcProbeData>::attach(offset) };
    diag_raw_print(
        b"[shared_arc_probe] child ATTACHED strong=0x",
        attached.strong_count(),
        b" magic=0x",
        attached.magic,
    );
    let magic_ok = attached.magic == SHARED_ARC_PROBE_MAGIC;
    diag_raw_print(
        b"[shared_arc_probe] child magic_match=0x",
        usize::from(magic_ok),
        b" counter_before=0x",
        attached.counter.load(Ordering::Acquire),
    );
    let observed = attached.counter.fetch_add(1, Ordering::AcqRel) + 1;
    diag_raw_print(
        b"[shared_arc_probe] child counter_after_fetch_add=0x",
        observed,
        b" strong=0x",
        attached.strong_count(),
    );
    let extra = attached.clone();
    diag_raw_print(
        b"[shared_arc_probe] child CLONED strong=0x",
        extra.strong_count(),
        b" counter=0x",
        extra.counter.load(Ordering::Acquire),
    );
    drop(extra);
    diag_raw_print(
        b"[shared_arc_probe] child DROPPED clone strong=0x",
        attached.strong_count(),
        b" counter=0x",
        attached.counter.load(Ordering::Acquire),
    );
    // `attached` itself is intentionally leaked here (never dropped): this diagnostic child
    // process is short-lived and about to continue into normal guest startup, and the whole
    // point of the probe is to demonstrate the handle stays valid and correctly counted for the
    // rest of this process's life, exactly the real `GlobalState`/`LiteBoxX` usage shape a future
    // migration needs -- an artificial extra drop here would prove nothing further.
    core::mem::forget(attached);
}

impl litebox::mm::allocator::MemoryProvider for WindowsUserland {
    fn alloc(layout: &std::alloc::Layout) -> Option<(usize, usize)> {
        let size = core::cmp::max(
            layout.size().next_power_of_two(),
            // Note `mmap` provides no guarantee of alignment, so we double the size to ensure we
            // can always find a required chunk within the returned memory region.
            core::cmp::max(layout.align(), 0x1000) << 1,
        );

        // **Reverted to private per-process `VirtualAlloc2`, 2026-09-17** (selective-routing
        // correction; exact pre-`c08182d` mechanism -- see [`SLAB_ALLOC`]'s doc comment for the
        // full story of why routing every host-heap allocation through the fixed-base shared
        // kernel section, tried 2026-09-16/17, is not viable: it exhausted that section's bump
        // allocator, which has no reclaim, after 45-90 real execs under real desktop load). Every
        // ORDINARY allocation this process's global allocator ever hands out -- including
        // one-shot buffers like OCI rootfs-reconstruction data during `exec` -- goes back to
        // normal private per-process memory here, exactly as before Track B step 3. Constrain
        // every host-allocator-backing page to `HOST_ALLOCATOR_REGION_MIN..`, strictly above the
        // guest's own `TASK_ADDR_MAX`, so this allocator can never be handed an address the
        // guest's `Vmem` also considers fair game. See `HOST_ALLOCATOR_REGION_MIN`'s doc comment
        // for why an unconstrained (null-base) request was unsafe here.
        let mut addr_req = MEM_ADDRESS_REQUIREMENTS {
            LowestStartingAddress: HOST_ALLOCATOR_REGION_MIN as *mut c_void,
            HighestEndingAddress: core::ptr::null_mut(),
            Alignment: 0,
        };
        let mut ext_param = MEM_EXTENDED_PARAMETER {
            Anonymous1: MEM_EXTENDED_PARAMETER_0 {
                _bitfield: MemExtendedParameterAddressRequirements as u64,
            },
            Anonymous2: windows_sys::Win32::System::Memory::MEM_EXTENDED_PARAMETER_1 {
                Pointer: (&raw mut addr_req).cast::<c_void>(),
            },
        };

        let result = match unsafe {
            VirtualAlloc2(
                GetCurrentProcess(),
                core::ptr::null_mut(),
                size,
                Win32_Memory::MEM_COMMIT | Win32_Memory::MEM_RESERVE,
                Win32_Memory::PAGE_READWRITE,
                &raw mut ext_param,
                1,
            )
        } {
            addr if addr.is_null() => None,
            addr => Some((addr as usize, size)),
        };

        if diag_alloc_enabled() {
            if let Some((addr, _)) = result {
                let n = DIAG_ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
                let offset = addr.wrapping_sub(HOST_ALLOCATOR_REGION_MIN);
                diag_raw_print(b"[diag_alloc] n=0x", n, b" off=0x", offset);
                diag_raw_print(
                    b"[diag_alloc]   size=0x",
                    size,
                    b" layout_size=0x",
                    layout.size(),
                );
            }
        }
        result
    }

    unsafe fn free(addr: usize) {
        // `addr` is guaranteed by the `MemoryProvider` contract to be a base address
        // previously returned by `alloc`, i.e. the base of a whole `VirtualAlloc2`
        // RESERVE|COMMIT region. `MEM_RELEASE` requires exactly that: the original
        // base address and a size of 0 (it always releases the entire region).
        let ok = unsafe { VirtualFree(addr as *mut c_void, 0, Win32_Memory::MEM_RELEASE) } != 0;
        assert!(ok, "VirtualFree(RELEASE) failed: {}", unsafe {
            GetLastError()
        });
    }
}

unsafe extern "C" {
    // Defined in asm blocks above
    fn syscall_callback() -> isize;
    fn exception_callback() -> isize;
    fn interrupt_callback();
    fn switch_to_guest_start();
    fn switch_to_guest_end();
}

unsafe extern "C-unwind" fn init_handler(thread_ctx: &mut ThreadContext<'_>) {
    // Pre-commit the real Windows stack pages below `host_sp` that
    // `vectored_exception_handler`'s `EXCEPTION_RECORD_RESERVE`-relative scratch write later
    // depends on. Guest threads have been observed reaching their first exception with as
    // little as 12KB of their 8MiB stack reservation actually committed, letting that write
    // land outside committed memory (`INVALID_POINTER_WRITE_c0000005_VCRUNTIME140.dll!memcpy`).
    // Touching each page here (before any guest code runs) forces Windows to commit it.
    {
        let host_sp = thread_ctx.tls.host_sp.get().cast::<u8>();
        let page_size = 4096usize;
        let mut offset = page_size;
        while offset <= EXCEPTION_RECORD_RESERVE {
            let probe_addr = host_sp.wrapping_byte_sub(offset);
            unsafe {
                core::ptr::write_volatile(probe_addr, core::ptr::read_volatile(probe_addr));
            }
            offset += page_size;
        }
    }
    thread_ctx.call_shim(|shim, ctx, _interrupt| shim.init(ctx));
}

unsafe extern "C-unwind" fn syscall_handler(thread_ctx: &mut ThreadContext<'_>) {
    // Repair `GS_BASE` here too, not only inside `vectored_exception_handler` (see
    // `WindowsUserland::restore_thread_gs_base_if_cleared`'s doc comment and the investigation
    // this call site is part of): every guest syscall is a guaranteed, high-frequency kernel
    // round-trip on real host stack, running strictly BEFORE any GS-dependent Windows code this
    // syscall's own handling might reach (heap allocation, `ntdll` calls, etc.) -- unlike the VEH
    // path, which can only repair GS_BASE for exceptions Windows' OWN exception dispatcher
    // successfully delivered, and that dispatcher itself needs a valid GS_BASE to locate the TEB
    // and route to a registered handler at all (confirmed live: a `0xc000000d` crash inside
    // `ntdll.dll` itself, at the `mov %gs:0x60, %rcx` TEB->PEB access, with none of litebox's own
    // VEH-side diagnostics ever firing first -- the OS's own fault delivery broke before litebox's
    // handler could run). Checking here closes that gap for the syscall path specifically: if
    // GS_BASE is corrupted by the time a syscall is dispatched, this repairs it before this
    // function calls into anything GS-dependent, rather than only reactively repairing after an
    // exception Windows may not have been able to deliver in the first place.
    WindowsUserland::restore_thread_gs_base_if_cleared();
    thread_ctx.call_shim(|shim, ctx, _interrupt| shim.syscall(ctx));
}

unsafe extern "C-unwind" fn exception_handler(
    thread_ctx: &mut ThreadContext<'_>,
    exception_record: &EXCEPTION_RECORD,
) {
    // Temporary (PASS 59, see FINDINGS.txt): `LITEBOX_DIAG_MALLOCNG=1`-gated dump of the mallocng
    // `free()` self-consistency check's operands (`rax = [rdi-0x10]` = the group pointer loaded
    // from the chunk header, `rcx = rdi-0x10` = the self-address the group's own `->self`-shaped
    // field at `[rax+0x10]` is expected to equal) right before the `0xc0000096` (privileged
    // instruction, i.e. `hlt`) catch-all panic below. Directly reads both the expected value
    // (`rcx`) and the actual value found at `[rax+0x10]` at the moment of the trap, without
    // needing a second run or a hardware watchpoint -- distinguishes "the slot holds a stale
    // pointer" (a translatable/healable value) from "the slot holds something not pointer-shaped
    // at all" (a wrong-length-copy/layout bug, pass 43's alternate hypothesis) directly.
    if exception_record.ExceptionCode == 0xc0000096u32.cast_signed()
        && std::env::var_os("LITEBOX_DIAG_MALLOCNG").is_some()
    {
        let ctx = &*thread_ctx.ctx;
        let rdi = ctx.rdi;
        let rax = ctx.rax;
        let rcx = ctx.rcx;
        // `rdi`/`rax` are ordinary guest register values at trap time, not guaranteed to hold a
        // valid pointer (confirmed live: `rax == 0` for one crash this diagnostic was used to
        // investigate, making the un-guarded `rax + 0x10` dereference below itself crash the
        // diagnostic pass with an unrelated access violation). Use the same fault-tolerant
        // primitive `fork_verify`'s own diagnostics already rely on instead of a raw
        // dereference, so a not-pointer-shaped register value reports as `None` here rather than
        // taking down the process the diagnostic was trying to observe.
        let group_ptr = fork_verify::read_stack_word_for_diagnostics(rdi.wrapping_sub(0x10));
        let self_slot_addr = rax.wrapping_add(0x10);
        let self_slot_value = fork_verify::read_stack_word_for_diagnostics(self_slot_addr);
        // Pass N: this trap is mallocng's basic 16-byte pointer-alignment check on `rdi` itself
        // (`test dil, 0xf` right before the `hlt`), not the `free()` self-pointer check this
        // block's own doc comment above describes -- `rdi & 0xf` is the actual failing
        // condition. Reverse-translate `rdi` (a CHILD/dest-space address, since this trap fires
        // on a fork() child under active verification) back to the PARENT's own pre-fork address
        // to determine whether the misalignment already existed before `fork()` duplicated this
        // memory (a genuine guest-side issue) or was introduced by litebox's own duplication/
        // healing (a litebox bug) -- must run before `end_fork_child_verification()` clears the
        // relocation map this needs.
        let rdi_source = get_tls_ptr().and_then(|tls| {
            fork_verify::reverse_translate_and_read_for_diagnostics(unsafe { &*tls }, rdi)
        });
        eprintln!(
            "[diag-mallocng] tid={:?} rdi={rdi:#x} rdi&0xf={:#x} rax={rax:#x} rcx={rcx:#x} \
             [rdi-0x10]={group_ptr:#x?} self_slot_addr(rax+0x10)={self_slot_addr:#x} \
             self_slot_value={self_slot_value:#x?} expected(rcx)={rcx:#x} match={} \
             rdi_source_addr={:#x?}",
            std::thread::current().id(),
            rdi & 0xf,
            self_slot_value == Some(rcx),
            rdi_source.map(|(source_addr, _)| source_addr),
        );
    }
    let (exception, error_code, cr2) = match exception_record.ExceptionCode {
        Win32_Foundation::EXCEPTION_ACCESS_VIOLATION => {
            let info = exception_record.ExceptionInformation;
            let read_write_flag = info[0];
            let faulting_address = info[1];
            if read_write_flag == 0 && faulting_address == !0 {
                // This is probably a #GP, not a #PF.
                (Exception::GENERAL_PROTECTION_FAULT, 0, 0)
            } else {
                // Windows' `ExceptionInformation[0]` is read(0)/write(1)/DEP-execute(8) --
                // the same convention already documented in this repo at
                // `process_fork.rs:2067` and `fork_verify.rs:2296` -- never a present/absent
                // bit, so bit0 (the real x86 P-bit) cannot be read off it and was previously
                // hardcoded to 0 (always "not present"), making every reported not-present
                // fault indistinguishable from a present-page protection violation. Recover
                // the real P-bit the same way `vectored_exception_handler`'s
                // `LITEBOX_DIAG_FAULT_VQ` diagnostic already does: ask Windows' own VAD tree
                // via `VirtualQuery` whether the faulting address is currently committed.
                let present = {
                    let mut mbi = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
                    let queried = unsafe {
                        Win32_Memory::VirtualQuery(
                            faulting_address as *const c_void,
                            &mut mbi,
                            core::mem::size_of::<Win32_Memory::MEMORY_BASIC_INFORMATION>(),
                        )
                    };
                    queried != 0 && mbi.State == Win32_Memory::MEM_COMMIT
                };
                // `read_write_flag == 8` is Windows' DEP/execute-prevention code, not a write
                // -- the previous `!= 0` test folded it into the write bit, fabricating a
                // write fault out of an instruction fetch. Emit the real instruction-fetch
                // bit (bit 4) instead, and set the write bit (bit 1) only for an actual write.
                let error_code: u32 = u32::from(present) // bit 0: present
                    | (1 << 2) // bit 2: user mode (this platform never delivers kernel-mode guest faults)
                    | if read_write_flag == 1 { 1 << 1 } else { 0 } // bit 1: write
                    | if read_write_flag == 8 { 1 << 4 } else { 0 }; // bit 4: instruction fetch (DEP)
                (Exception::PAGE_FAULT, error_code, faulting_address)
            }
        }
        Win32_Foundation::EXCEPTION_ILLEGAL_INSTRUCTION => (Exception::INVALID_OPCODE, 0, 0),
        Win32_Foundation::EXCEPTION_BREAKPOINT => (Exception::BREAKPOINT, 0, 0),
        Win32_Foundation::EXCEPTION_INT_DIVIDE_BY_ZERO => (Exception::DIVIDE_ERROR, 0, 0),
        // `STATUS_PRIVILEGED_INSTRUCTION` (0xc0000096): Windows' name for trapping an `hlt`
        // executed at CPL3. On real x86_64/Linux, an unprivileged `hlt` raises vector 6 (`#UD`,
        // Invalid Opcode) -- the exact trap musl's `a_crash()` (mallocng's heap-integrity-assert
        // abort primitive, `src/malloc/mallocng/*.c`) deliberately executes on a failed
        // assertion. Before this arm existed, this code reached the catch-all `panic!` below
        // instead of being delivered to the guest as `SIGILL` like real Linux would: harmless to
        // the host process itself (the panic unwinds and the OS thread's own outcome is usually
        // unobserved) but printed spurious "Unhandled Win32 exception" panic noise to stderr on
        // every mallocng-assert trap, including ones the guest's own signal handling could
        // otherwise report/recover from normally.
        code if code == 0xc0000096u32.cast_signed() => (Exception::INVALID_OPCODE, 0, 0),
        code => panic!("Unhandled Win32 exception code: {code:#x}"),
    };

    let info = litebox::shim::ExceptionInfo {
        exception,
        error_code,
        cr2,
        kernel_mode: false,
    };

    thread_ctx.call_shim(|shim, ctx, _interrupt| shim.exception(ctx, &info));
}

unsafe extern "C-unwind" fn interrupt_handler(thread_ctx: &mut ThreadContext<'_>) {
    thread_ctx.tls.is_in_guest.set(false);
    litebox_util_log::debug!(
        tid:? = std::thread::current().id();
        "drm-diag: interrupt_handler entry"
    );
    thread_ctx.call_shim(|shim, ctx, interrupt| {
        litebox_util_log::debug!(
            tid:? = std::thread::current().id(), interrupt:% = interrupt;
            "drm-diag: interrupt_handler call_shim closure"
        );
        if interrupt {
            shim.interrupt(ctx)
        } else {
            // We likely got here just to restore fsbase, so don't bother the
            // shim.
            ContinueOperation::Resume
        }
    });
}

struct ThreadContext<'a> {
    shim: &'a dyn litebox::shim::EnterShim<ExecutionContext = litebox_common_linux::PtRegs>,
    ctx: &'a mut litebox_common_linux::PtRegs,
    tls: &'a TlsState,
}

impl ThreadContext<'_> {
    /// Calls `f` in order to call into a shim entrypoint.
    fn call_shim(
        &mut self,
        f: impl FnOnce(
            &dyn litebox::shim::EnterShim<ExecutionContext = litebox_common_linux::PtRegs>,
            &mut litebox_common_linux::PtRegs,
            bool,
        ) -> ContinueOperation,
    ) {
        // Clear the interrupt flag before calling the shim, since we've handled it
        // now (by calling into the shim), and it might be set again by the shim
        // before returning.
        let op = f(self.shim, self.ctx, self.tls.interrupt.replace(false));
        match op {
            ContinueOperation::Resume => {
                // Diagnostic-only (`LITEBOX_CTXWATCH=1`): arm a hardware write-watchpoint on
                // this thread's own `ctx.rip` field right before resuming into the guest,
                // conditioned on `orig_rax == 0x3d` (wait4) to minimize overhead and match the
                // exact syscall this bug's crashes have consistently followed. See `ctxwatch`
                // for the full rationale. `vectored_exception_handler` disarms it again the next
                // time this thread leaves guest mode.
                // Temporary (see FINDINGS.txt PASS 48, revised PASS 54): arm the fixed-address
                // `Dr1` watch on this thread's first resume -- cheap (no-op unless
                // `LITEBOX_DIAG_WATCHADDR` is set) and self-limiting to once per thread (see
                // `ctxwatch::State::fixed_armed`), guaranteeing it is set before the very first
                // guest instruction runs on this thread without re-arming (and paying a
                // `GetThreadContext`/`SetThreadContext` round-trip) on every subsequent syscall
                // return for that thread's whole lifetime, which pass 53 found perturbs some
                // repros' timing badly enough to prevent them ever reaching the code being
                // watched.
                ctxwatch::arm_fixed_on_current_thread();
                if ctxwatch::enabled() && self.ctx.orig_rax == 0x3d {
                    ctxwatch::arm(self.ctx);
                    // Debug registers are per-thread on Windows (virtualized via
                    // Get/SetThreadContext): a watchpoint armed only on this (the shell's own)
                    // thread can never observe a write performed by an instruction executing on
                    // a DIFFERENT OS thread, e.g. a pipeline child's own exit/teardown code
                    // running on its own thread. Arm the identical watchpoint on every other
                    // live thread too, reusing the same suspend/set-context/resume pattern
                    // `ThreadHandle::interrupt` already uses for cross-thread context
                    // manipulation.
                    ctxwatch_arm_other_threads(self.ctx);
                }
                // A corrupted context kills THIS GUEST PROCESS, not the host.
                //
                // This used to reach `switch_to_guest`'s assertion and panic the whole runner --
                // and with every guest process sharing one host process, that took down the entire
                // desktop because one component's context went bad. The assertion's own comment
                // already said what should happen instead: "a real Linux kernel would deliver
                // SIGSEGV to a userspace program that corrupts its own signal frame this way
                // rather than crash the kernel itself; this project does not yet synthesize that
                // signal at this choke point (a follow-up)". This is that follow-up.
                //
                // Synthesizing a page fault at `rip` and handing it to the shim reuses the path
                // that already exists for a genuine guest fault, so the outcome is exactly what
                // Linux gives: a handler runs if the guest installed one, and otherwise the task
                // dies with SIGSEGV and its parent reaps it. Observed live as
                // `rip=0x7ff8b1b221f4` -- a Windows DLL address -- in a context about to be
                // resumed as guest code.
                //
                // The `kernel_mode: false` and `error_code: 0x14` (user-mode instruction fetch of
                // a non-present page) describe what actually went wrong: control was about to be
                // transferred to an address the guest cannot execute.
                if !guest_context_is_plausible(self.ctx) {
                    litebox_util_log::error!(
                        rip:% = self.ctx.rip, rsp:% = self.ctx.rsp;
                        "implausible guest context on resume -- delivering SIGSEGV to the guest \
                         task instead of terminating the host"
                    );
                    let info = litebox::shim::ExceptionInfo {
                        exception: litebox::shim::Exception::PAGE_FAULT,
                        error_code: 0x14,
                        cr2: self.ctx.rip,
                        kernel_mode: false,
                    };
                    match self.shim.exception(self.ctx, &info) {
                        // The shim ran a guest signal handler and gave us a fresh context. Resume
                        // only if THAT one is sane; otherwise let the task end rather than loop.
                        ContinueOperation::Resume if guest_context_is_plausible(self.ctx) => {
                            unsafe { switch_to_guest(self.ctx) }
                        }
                        _ => return,
                    }
                }
                unsafe { switch_to_guest(self.ctx) }
            }
            ContinueOperation::Terminate => {}
        }
    }
}

impl litebox::platform::ForkChildVerificationProvider for WindowsUserland {
    fn begin_fork_child_verification(&self, relocations: Arc<litebox::mm::AddressRelocations>) {
        fork_verify::begin(relocations);
    }

    fn end_fork_child_verification(&self) {
        fork_verify::end();
    }

    fn lock_fork_verify_heal(&self) -> impl Sized {
        // Re-entrant blocking acquisition, same as the exception handler's -- see
        // `lock_fork_verify_heal_reentrant`. The old bounded spin was justified by this running on
        // the PARENT's thread inside `do_clone`, which "must never risk deadlocking against a
        // healing pass"; the deadlock it feared is same-thread re-entry, which the depth count now
        // handles directly, so waiting for a DIFFERENT thread's bounded healing pass is simply
        // correct rather than dangerous.
        let guard = lock_fork_verify_heal_reentrant();
        guard
    }

    fn current_thread_fork_relocations(&self) -> Option<Arc<litebox::mm::AddressRelocations>> {
        let tls = get_tls_ptr()?;
        // SAFETY: `get_tls_ptr` returns this thread's live `TlsState`.
        let tls = unsafe { &*tls };
        tls.fork_verify.borrow().clone()
    }

    fn diagnostic_reverse_translate_fork_child_addr(&self, dest_addr: usize) -> Option<usize> {
        let tls = get_tls_ptr()?;
        // SAFETY: `get_tls_ptr` returns this thread's live `TlsState`.
        let tls = unsafe { &*tls };
        fork_verify::reverse_translate_and_read_for_diagnostics(tls, dest_addr)
            .map(|(source_addr, _bytes)| source_addr)
    }

    fn diagnostic_process_fork_probe(
        &self,
        relocations: &litebox::mm::AddressRelocations,
        fd_complexity: litebox::platform::ForkFdComplexity,
        translated_gprs: Option<litebox::platform::ForkGprSnapshot>,
        full_translated_gprs: Option<litebox::platform::ForkFullGprSnapshot>,
    ) {
        // Independent of the spawn probe's own gate below (pass 116): purely logs the fd-table
        // classification `do_clone` already computed, never touches the spawn/resume/fds probes'
        // own state, and is itself inert unless its own env var is set.
        if process_fork::diag_process_fork_fd_complexity_enabled() {
            eprintln!(
                "[process_fork_diag] fork(): fd-complexity total_alive={} beyond_stdio={} ({})",
                fd_complexity.total_alive,
                fd_complexity.beyond_stdio,
                if fd_complexity.beyond_stdio == 0 {
                    "simple: process-based fork could inherit this fd table today"
                } else {
                    "complex: process-based fork would need to fall back to thread-based fork for this fd table"
                }
            );
        }
        if !process_fork::diag_process_fork_spawn_enabled() {
            return;
        }
        let group_relocations = relocations.group_relocations();
        eprintln!(
            "[process_fork_diag] fork(): probing {} reservation group(s) against a real, inert CreateProcess child",
            group_relocations.len()
        );
        // Read each group's real, live bytes out of THIS (the parent) process -- same primitive
        // `Vmem::duplicate` itself uses (`RawConstPointer::to_owned_slice`), just invoked here
        // for a diagnostic side channel rather than the real duplication path.
        // Read a copy group PAGE AT A TIME, tolerating pages that are not mapped.
        //
        // A copy group is a 64 KiB-granule-aligned span covering one or more guest regions (see
        // the group construction in `do_clone`'s `try_cross_process_fork`), so by construction it
        // can include padding that no guest mapping covers -- and it can also span a guard gap or
        // a range the guest `munmap`ed between two regions it does cover. The previous
        // whole-range `to_owned_slice(range.len())` assumed every byte was readable and faulted in
        // the PARENT, mid-fork, before the child was ever resumed.
        //
        // Unreadable pages contribute zeroes, which is the correct content for them: the guest
        // cannot have observed anything there either, and the child re-establishes each region's
        // real bounds and permissions from the VMA layout, which travels separately.
        let read_source_bytes = |range: core::ops::Range<usize>| {
            use litebox::platform::RawConstPointer as _;
            let ptr =
                <Self as litebox::platform::RawPointerProvider>::RawConstPointer::<u8>::from_usize(
                    range.start,
                );
            ptr.to_owned_slice(range.len()).map(<[u8]>::into_vec)
        };
        let want_registers = process_fork::diag_process_fork_registers_enabled();
        let inject_gprs = if want_registers {
            translated_gprs
        } else {
            None
        };
        let want_real_resume = process_fork::diag_process_fork_real_resume_enabled();
        let want_task_resume = process_fork::diag_process_fork_task_resume_enabled();
        let inject_full_gprs = if want_real_resume || want_task_resume {
            full_translated_gprs
        } else {
            None
        };
        // Pass 122: serialize the REAL `AddressRelocations` this actual `fork()` call just built
        // (the same object the working thread-based path hands to `fork_verify::begin` today) so
        // the cross-process diagnostic child can reconstruct an equivalent object and arm its own
        // `fork_verify` -- see `diag_process_fork_relocations_enabled`'s doc comment for why this
        // is sound for this diagnostic's identity-address design. Computed unconditionally
        // (cheap: a handful of VMAs in every repro this investigation has run) and gated inside
        // `diagnostic_spawn_and_copy` itself, matching every other probe's own style.
        let relocations_line = Some(relocations.serialize_for_diagnostic());
        match process_fork::diagnostic_spawn_and_copy(
            group_relocations,
            read_source_bytes,
            inject_gprs,
            inject_full_gprs,
            relocations_line,
        ) {
            Ok(results) => {
                let succeeded = results.iter().filter(|r| r.succeeded).count();
                eprintln!(
                    "[process_fork_diag] fork(): {succeeded}/{} group(s) succeeded",
                    results.len()
                );
                for r in &results {
                    if r.succeeded {
                        eprintln!(
                            "[process_fork_diag]   OK    group={:#x}..{:#x} (len={:#x})",
                            r.source_group.start,
                            r.source_group.end,
                            r.source_group.len()
                        );
                    } else {
                        eprintln!(
                            "[process_fork_diag]   FAIL  group={:#x}..{:#x} (len={:#x}) GetLastError={}",
                            r.source_group.start,
                            r.source_group.end,
                            r.source_group.len(),
                            r.last_error
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!("[process_fork_diag] fork(): probe setup failed: {e}");
            }
        }
    }

    fn wait_for_cross_process_exit(
        &self,
        handle: litebox::platform::CrossProcessChildHandle,
    ) -> u32 {
        // Pass 59: `handle.0` is the child TASK's initiating THREAD handle (registered by
        // `spawn_cross_process_fork_child`, `lib.rs`), not the Windows PROCESS handle this used
        // to be -- waiting on the whole process silently never signals once the child spawns any
        // further OS thread of its own that outlives the original task (see
        // `process_fork::wait_for_thread_exit`'s doc comment for the full mechanism and the real
        // `ssh-agent` case that surfaced it). `diagnostic_cross_process_wait4_probe`'s own
        // self-test is the one remaining PROCESS-handle producer/consumer pair, deliberately kept
        // on the separate `process_fork::wait_for_process_exit`/`try_wait_for_process_exit`
        // functions -- never routed through here.
        //
        // Safety: `handle.0` is a `HANDLE` value registered via `Process::register_cross_process_child`,
        // which only ever receives a real, currently-open thread handle here, not yet closed --
        // the registry entry's removal (`sys_wait4`'s `reap_cross_process_child`) is the only
        // thing that ever invalidates it, and that always happens strictly after this call.
        unsafe {
            process_fork::wait_for_thread_exit(handle.0 as windows_sys::Win32::Foundation::HANDLE)
        }
    }

    fn try_wait_for_cross_process_exit(
        &self,
        handle: litebox::platform::CrossProcessChildHandle,
    ) -> Option<u32> {
        // Safety: same contract as `wait_for_cross_process_exit` above.
        unsafe {
            process_fork::try_wait_for_thread_exit(
                handle.0 as windows_sys::Win32::Foundation::HANDLE,
            )
        }
    }

    fn spawn_cross_process_exit_notifier(
        &'static self,
        handle: litebox::platform::CrossProcessChildHandle,
        on_exit: alloc::boxed::Box<dyn FnOnce() + Send>,
    ) {
        // This thread's only job is the blocking wait -- `wait_for_cross_process_exit` already
        // does it, same `HANDLE`-safety contract as every other cross-process-wait call above
        // (the registry entry outlives this thread; `sys_wait4`'s `reap_cross_process_child`
        // only ever runs after a wait on this same handle has already returned). Deliberately NOT
        // using `self.global.platform`-style access to anything guest-side here: this thread
        // exists specifically to reach into the GUEST's own notify machinery through `on_exit`,
        // never to touch guest memory or state directly itself.
        std::thread::spawn(move || {
            // 53rd-pass diagnostic (AGENTS_ARCHIVE_2026-09-22.md pickup): bracket the blocking
            // wait with explicit start/end markers, keyed by the handle's raw pointer value, so a
            // trace can pair this thread's own timing against `try_wait_for_cross_process_exit`
            // polls elsewhere and against the guest-visible dbus activation-failure log line.
            eprintln!(
                "[wait4_diag] arm_cross_process_exit_notifier: background wait thread starting, handle={:p}",
                handle.0 as windows_sys::Win32::Foundation::HANDLE
            );
            self.wait_for_cross_process_exit(handle);
            eprintln!(
                "[wait4_diag] arm_cross_process_exit_notifier: blocking wait returned, invoking on_exit(), handle={:p}",
                handle.0 as windows_sys::Win32::Foundation::HANDLE
            );
            on_exit();
        });
    }

    fn diagnostic_cross_process_wait4_probe(
        &self,
        register: &mut dyn FnMut(i32, litebox::platform::CrossProcessChildHandle),
    ) {
        process_fork::diagnostic_cross_process_wait4_probe(register);
    }

    fn take_cross_process_writable_layer_export(
        &self,
        handle: litebox::platform::CrossProcessChildHandle,
    ) -> Option<alloc::vec::Vec<u8>> {
        // The child's own export path is derived from its REAL Windows pid (see
        // `process_fork::cross_process_writable_export_path`'s doc comment) -- recover that pid
        // from the still-open THREAD `HANDLE` this registry entry carries (pass 59: no longer a
        // process handle -- see `wait_for_cross_process_exit`'s doc comment) via
        // `GetProcessIdOfThread`, the thread-handle counterpart of the `GetProcessId` this used
        // to call.
        let raw_handle = handle.0 as windows_sys::Win32::Foundation::HANDLE;
        let pid = unsafe { windows_sys::Win32::System::Threading::GetProcessIdOfThread(raw_handle) };
        if pid == 0 {
            return None;
        }
        let tar_path = std::env::var_os(process_fork::FORK_CHILD_TAR_PATH_ENV_VAR)?;
        let export_path =
            process_fork::cross_process_writable_export_path(std::path::Path::new(&tar_path), pid);
        let bytes = std::fs::read(&export_path).ok()?;
        // The export is single-use: once read back into the parent, remove it so a later,
        // unrelated child that happens to reuse the same (now-recycled) pid never sees a stale
        // archive left over from a previous run.
        let _ = std::fs::remove_file(&export_path);
        Some(bytes)
    }

    fn spawn_cross_process_fork_child(
        &'static self,
        relocations: &litebox::mm::AddressRelocations,
        full_gprs: litebox::platform::ForkFullGprSnapshot,
        inherited_pipes: std::vec::Vec<(i32, litebox::platform::ForkPipeBridge)>,
        inherited_files: std::vec::Vec<litebox::platform::ForkInheritedFile>,
        inherited_eventfds: std::vec::Vec<litebox::platform::ForkInheritedEventfd>,
    ) -> Option<litebox::platform::CrossProcessChildHandle> {
        std::env::var_os("LITEBOX_PROCESS_FORK")?;
        let group_relocations = relocations.group_relocations();
        let vma_layout = relocations.vma_layout();
        // Read PAGE AT A TIME and refuse to touch a page that is not committed.
        //
        // `copy_one_group` already calls this once per page and treats `None` as "leave the
        // child's zero-fill alone", but the read itself used to assume the page was mapped. A
        // copy group is widened out to 64 KiB allocation granularity, and litebox commits guest
        // memory on demand, so a group legitimately contains `MEM_RESERVE`-but-not-committed
        // pages -- reading one faults in the PARENT, mid-fork. Measured: the copy walks ~199
        // pages of the first group and then hits `0x10106000`, which `VirtualQuery` reports as
        // `State=MEM_RESERVE Protect=0x0`, and the run dies there every time.
        //
        // `fork_verify::readable_region` is the same committed-and-readable `VirtualQuery` test
        // `is_readable` uses elsewhere, so an uncommitted page is skipped rather than faulted on
        // -- but it ALSO returns the queried region's full bounds, cached here across calls
        // (`cached_region`) so consecutive pages within the SAME real guest mapping (the common
        // case -- a mapping is typically megabytes, not one page) pay for one `VirtualQuery` per
        // region instead of one per page. Measured live: this was the actual dominant cost of
        // copying a large guest process's memory across a cross-process fork (23-25s for one
        // 173MB region, ~42,000 pages) -- see `readable_region`'s own doc comment for the full
        // finding and why batching the WRITE side instead (tried first) did not help.
        let mut cached_region: Option<core::ops::Range<usize>> = None;
        let read_source_bytes = |range: core::ops::Range<usize>| {
            use litebox::platform::RawConstPointer as _;
            const PAGE: usize = litebox::mm::linux::PAGE_SIZE;
            let len = range.len();
            let mut out = std::vec::Vec::new();
            out.resize(len, 0u8);
            let mut any_readable = false;
            let mut off = 0usize;
            while off < len {
                let addr = range.start.wrapping_add(off);
                let chunk = (PAGE - (addr % PAGE)).min(len - off);
                let in_cached_region = cached_region.as_ref().is_some_and(|r| r.contains(&addr));
                if !in_cached_region {
                    cached_region = fork_verify::readable_region(addr);
                }
                if cached_region.is_some() {
                    let ptr = <Self as litebox::platform::RawPointerProvider>::RawConstPointer::<
                        u8,
                    >::from_usize(addr);
                    if let Some(bytes) = ptr.to_owned_slice(chunk) {
                        out[off..off + chunk].copy_from_slice(&bytes);
                        any_readable = true;
                    }
                }
                off += chunk;
            }
            any_readable.then_some(out)
        };
        // PASS 150: the child's own `fork_verify` reads this line back and calls `translate()` on
        // it to repair stale pointers it observes mid-execution -- but the cross-process child's
        // real memory always lives at SOURCE coordinates (`copy_one_group` never uses `dest_base`
        // to place bytes), so the child must see an IDENTITY relocation map, not the thread-based
        // path's own `dest_base` values. See `AddressRelocations::identity_for_cross_process`'s
        // doc comment for the exact fault this fixes (a repeating `EXECUTE/DEP` instruction-fetch
        // crash from `fork_verify` "healing" a live `rip` into an unmapped thread-based-path
        // destination address).
        let relocations_line = relocations
            .identity_for_cross_process()
            .serialize_for_diagnostic();
        // Give every carried guest pipe fd a REAL Windows pipe the child inherits.
        //
        // Built before the spawn because `CreateProcessW` is what actually transfers the handles,
        // and their values have to be in the child's environment block by then. The parent keeps
        // only its own end; `spawn_process_fork_child` closes the child-side handles as soon as
        // the spawn is decided (see its own comment on why holding one would turn EOF into a
        // hang).
        let mut child_pipe_handles: std::vec::Vec<(
            i32,
            windows_sys::Win32::Foundation::HANDLE,
            process_fork::ChildPipeEnd,
        )> = std::vec::Vec::new();
        let mut pumps: std::vec::Vec<(usize, litebox::platform::ForkPipeBridge)> =
            std::vec::Vec::new();
        for (fd, bridge) in inherited_pipes {
            // The child inherits the end it will USE, which is the opposite of the parent-side end
            // this bridge holds: a `Sink` means the parent-side end is a writer, so the child is
            // the one writing into the OS pipe.
            let which = match &bridge {
                litebox::platform::ForkPipeBridge::Sink(_) => {
                    process_fork::ChildPipeEnd::ChildWrites
                }
                litebox::platform::ForkPipeBridge::Source(_) => {
                    process_fork::ChildPipeEnd::ChildReads
                }
            };
            match process_fork::create_inheritable_child_pipe(which) {
                Ok((local, child)) => {
                    child_pipe_handles.push((fd, child, which));
                    pumps.push((local as usize, bridge));
                }
                Err(e) => {
                    // Fall back rather than spawn a child that silently loses the fd. Everything
                    // built so far has to be released by hand -- there is no `Drop` guard on this
                    // path by design (see `spawn_process_fork_child`'s doc comment).
                    litebox_util_log::warn!(
                        err:% = e;
                        "spawn_cross_process_fork_child: could not create an inheritable pipe for a guest fd, caller should fall back to thread-based fork"
                    );
                    for (_, h, _) in &child_pipe_handles {
                        unsafe { windows_sys::Win32::Foundation::CloseHandle(*h) };
                    }
                    for (h, _) in &pumps {
                        unsafe {
                            windows_sys::Win32::Foundation::CloseHandle(
                                *h as windows_sys::Win32::Foundation::HANDLE,
                            )
                        };
                    }
                    return None;
                }
            }
        }

        match process_fork::spawn_process_fork_child(
            group_relocations,
            &vma_layout,
            read_source_bytes,
            full_gprs,
            relocations_line,
            &child_pipe_handles,
            &inherited_files,
            &inherited_eventfds,
        ) {
            Ok(Some((pid, process_handle, thread_handle))) => {
                litebox_util_log::debug!(
                    pid:% = pid;
                    "spawn_cross_process_fork_child: child spawned and resumed successfully"
                );
                for (local, bridge) in pumps {
                    spawn_fork_child_pipe_pump(local, bridge, process_handle as usize);
                }
                // Pass 59: registered by the child's own initiating THREAD handle, not its
                // Windows PROCESS handle -- see `wait_for_thread_exit`'s doc comment
                // (`process_fork.rs`) for why process-handle-based waiting silently never
                // signals once this child spawns any further OS thread of its own (e.g. its own
                // internal fork() falling back to the thread-based path) that outlives the
                // original guest task a parent's `wait4()` actually cares about.
                Some(litebox::platform::CrossProcessChildHandle(
                    thread_handle as usize,
                ))
            }
            Ok(None) => {
                litebox_util_log::warn!(
                    "spawn_cross_process_fork_child: spawn/resume failed, caller should fall back to thread-based fork"
                );
                close_unused_pipe_ends(pumps);
                None
            }
            Err(e) => {
                litebox_util_log::warn!(
                    err:% = e;
                    "spawn_cross_process_fork_child: setup error, caller should fall back to thread-based fork"
                );
                close_unused_pipe_ends(pumps);
                None
            }
        }
    }

    /// See the trait method's own doc comment for the full "real `execve()` guarantees a fresh
    /// address space; this process literally cannot provide one itself" rationale.
    ///
    /// Reuses the EXISTING rootfs-continuity mechanism wholesale rather than inventing a second
    /// one: `run()` already sets [`process_fork::FORK_CHILD_OCI_IMAGE_ENV_VAR`]/
    /// [`process_fork::FORK_CHILD_TAR_PATH_ENV_VAR`] on THIS process's own environment
    /// unconditionally (for the cross-process-fork child's benefit) -- `std::process::Command`
    /// inherits this process's environment by default, so the spawned child sees them with no
    /// new plumbing, and just needs telling, via an ORDINARY `--oci-image`/`--initial-files` CLI
    /// flag (clap does not bind flags to env vars on its own here), to use them the normal way
    /// any fresh `litebox_runner_linux_on_windows_userland` invocation would.
    ///
    /// Writable-layer continuity reuses the EXISTING cross-process-FORK mechanism wholesale, not
    /// a second one: [`process_fork::export_parent_writable_layer_for_child`] snapshots whatever
    /// this process has written so far to a tar, the same call the fork path already makes, and
    /// the child imports it the normal, public way any fresh run would (`--resume-from`) --
    /// without this, every collision child would start from the image's base rootfs, blind to
    /// every directory/file an EARLIER, in-process step of this same boot had already created.
    /// Confirmed live as a real (not hypothetical) gap: a later `s6-mkdir` collision child
    /// reported `/run/s6/basedir: No such file or directory` because an EARLIER, successful,
    /// in-process step's own `/run/s6` never reached it.
    ///
    /// **Disclosed limitation, v1.** Only stdio (fds 0/1/2, inherited the same way any ordinary
    /// child process's are) crosses this boundary -- no non-stdio fd carrying (pipes/eventfds/
    /// files, same machinery `spawn_cross_process_fork_child` already has) yet. Exactly
    /// [`LITEBOX_PROCESS_FORK_IGNORE_FDS`]'s own precedent: a narrower, disclosed trade-off is
    /// strictly better than the unconditional `SIGSEGV` this replaces, and widening it is real,
    /// separate follow-on work, not silently claimed here.
    fn spawn_exec_collision_child(
        &self,
        path: &str,
        argv: &[alloc::ffi::CString],
        envp: &[alloc::ffi::CString],
    ) -> Option<litebox::platform::ExecCollisionChildResult> {
        let exe = std::env::current_exe().ok()?;
        let mut cmd = std::process::Command::new(exe);

        if let Ok(image_ref) = std::env::var(process_fork::FORK_CHILD_OCI_IMAGE_ENV_VAR)
            && !image_ref.is_empty()
        {
            cmd.arg("--oci-image").arg(image_ref);
        } else if let Ok(tar_path) = std::env::var(process_fork::FORK_CHILD_TAR_PATH_ENV_VAR)
            && !tar_path.is_empty()
        {
            cmd.arg("--initial-files").arg(tar_path);
        } else {
            litebox_util_log::warn!(
                oci_image_env_var:% = process_fork::FORK_CHILD_OCI_IMAGE_ENV_VAR,
                tar_path_env_var:% = process_fork::FORK_CHILD_TAR_PATH_ENV_VAR;
                "spawn_exec_collision_child: neither env var is set on this process's own \
                 environment -- this process did not boot from an OCI image or a tar, so there is \
                 no rootfs to hand the child"
            );
            return None;
        }

        // Writable-layer continuity -- see this method's own doc comment for why this is not
        // optional in practice. Best-effort, same as the fork path's identical call: a failure
        // (nothing registered, or the export itself failing) means the child sees only the base
        // rootfs, strictly worse than a correct hand-off but never worse than the unconditional
        // `SIGSEGV` this whole method replaces.
        if let Some(snapshot) = process_fork::export_parent_writable_layer_for_child() {
            cmd.arg("--resume-from").arg(snapshot);
        }

        // The other half of writable-layer continuity (see this method's own doc comment): the
        // child exports whatever IT writes, to a SEPARATE path from the `--resume-from` one above
        // (the same archive cannot be both read at startup and overwritten at exit), which this
        // call reads back after the child exits and hands to the caller to import into the
        // CONTINUING guest process -- there is no later `wait4` to carry it at here, unlike the
        // cross-process FORK case this mirrors.
        //
        // MUST be added before any positional argument below: `program_and_arguments` is a
        // `trailing_var_arg` positional (clap), which greedily swallows every token after the
        // first one, flags included -- an `--export-writable-layer` placed after `path`/`argv`
        // would silently become part of the GUEST's own argv instead of being parsed as a flag
        // at all. Confirmed live: exactly that, no error, `cli_args.export_writable_layer` stayed
        // `None`, and nothing was ever written.
        // A per-call sequence number, not just this process's own pid: several guest THREADS of
        // this SAME process can each hit their own collision and call this concurrently, and a
        // pid-only name would let two of them share one file, each truncating/overwriting the
        // other's write mid-flight. Same pattern as `export_parent_writable_layer_for_child`'s
        // own `SEQ` for the identical reason.
        static EXEC_COLLISION_SEQ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        let export_path = std::env::temp_dir().join(format!(
            "litebox-execwrite-{}-{}.tar",
            std::process::id(),
            EXEC_COLLISION_SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        ));
        cmd.arg("--export-writable-layer").arg(&export_path);

        // `--env KEY=VALUE`, once per entry -- the NEW program's own guest-visible environment
        // (`execve`'s own `envp` argument), entirely separate from this HOST process's
        // environment (which the child inherits unconditionally via `Command`'s own default,
        // unrelated to this loop).
        //
        // `glibc_tunables_forwarded` is a targeted diagnostic for the ADVISORY-001 §3N
        // tcache/fastbin workaround specifically: that workaround's whole premise is that
        // `GLIBC_TUNABLES` reaches every guest process's OWN libc startup, and this collision-
        // recovery path is a genuinely separate host process spawn (`docs/AGENTS_ARCHIVE_
        // 2026-09-16.md`) where "does the caller's ambient env carry it" is not a valid
        // assumption -- only this entry's `envp`, forwarded above, does. Logging whether it was
        // actually present in that envp turns "the tunable might be getting dropped somewhere in
        // this fork/exec chain" from a re-derived guess into a one-line live fact on every
        // collision, at effectively zero cost (this path already logs on every occurrence, and
        // occurrences are rare relative to a boot's overall syscall volume).
        let mut glibc_tunables_forwarded: Option<bool> = None;
        for entry in envp {
            let Ok(entry) = entry.to_str() else {
                litebox_util_log::warn!(
                    "spawn_exec_collision_child: a guest envp entry was not valid UTF-8, dropping it"
                );
                continue;
            };
            if entry.starts_with("GLIBC_TUNABLES=") {
                glibc_tunables_forwarded = Some(true);
            }
            cmd.arg("--env").arg(entry);
        }
        litebox_util_log::warn!(
            path:% = path, glibc_tunables_forwarded:% = glibc_tunables_forwarded.unwrap_or(false);
            "spawn_exec_collision_child: GLIBC_TUNABLES presence in the envp forwarded to the \
             replacement process -- ADVISORY-001 §3N diagnostic"
        );

        // The new program's own path, then every argv entry EXCEPT argv[0] -- `program_and_
        // arguments`'s own doc comment: the path is given separately, and litebox supplies its
        // own argv[0] from it, so the guest's original argv[0] (conventionally a program name,
        // not necessarily `path` itself) is not re-passed here.
        cmd.arg(path);
        for arg in argv.iter().skip(1) {
            let Ok(arg) = arg.to_str() else {
                litebox_util_log::warn!(
                    "spawn_exec_collision_child: a guest argv entry was not valid UTF-8, dropping it"
                );
                continue;
            };
            cmd.arg(arg);
        }

        // This process already bound any published host ports; the child must not also try --
        // same reasoning, same override, as `process_fork`'s own `child_env` for a cross-process
        // FORK child (see its doc comment on `("LITEBOX_PUBLISH", String::new())`).
        cmd.env("LITEBOX_PUBLISH", "");
        // See this env var's own doc comment: lets the child's `run()` skip the host-wide boot
        // lock, which exists for a different case (two independent, accidental concurrent boots)
        // than this one (one deliberate, synchronous continuation of the SAME boot).
        cmd.env(process_fork::EXEC_COLLISION_CHILD_ENV_VAR, "1");

        litebox_util_log::warn!(
            path:% = path;
            "spawn_exec_collision_child: this process's own address space cannot load this \
             image (a fixed-address collision with a still-live guest process) -- spawning a \
             fresh process instead of killing the guest, matching real Linux's own execve() \
             guarantee of a fresh address space"
        );

        // Blocking: see the trait method's own doc comment for why there is nothing else for
        // this thread to do but wait -- but NOT unconditionally forever. Live-reproduced this
        // session (`docs/AGENTS_ARCHIVE_2026-09-15.md`'s webtop-boot investigation): a collision
        // child that itself needs a sibling guest process's AF_UNIX socket (the X11 display, the
        // D-Bus session bus) -- unreachable from this genuinely separate OS process, same
        // architectural gap `docs/fork-fs-veh-2026-09-08.md` already documents for the sibling
        // cross-process FORK mechanism -- can connect() against the filesystem path and then hang
        // forever waiting for a peer that will never answer, exactly the already-known
        // `dbus-daemon --fork` hang class this file's own module doc describes, just reached via
        // this different call path. Observed live: a `/lsiopy/bin/python3` (selkies' own
        // interpreter) collision child sat at 0% CPU for 7+ minutes with no further log line,
        // wedging the ENTIRE guest boot (the top-level shell's own supervisor loop never saw
        // `selkies` exit to respawn it) until manually killed. `cmd.status()` has no timeout at
        // all, so this call used to block the calling guest thread -- and therefore this one
        // collision recovery -- indefinitely.
        //
        // The fix: poll instead of blocking outright, using the SAME "genuinely wedged, not just
        // slow" CPU-progress check `run_external_fault_watchdog_child` (`process_fork.rs`) already
        // uses and already trusts for exactly this judgment call -- measured on the CHILD's handle
        // from THIS (a genuinely different) process, not the child measuring itself, which is the
        // specific self-measurement failure mode that function's own doc comment separately
        // disclosed and ruled out. A child making real CPU progress (a legitimate, if slow, full
        // nested image re-pull/merge/boot) is never killed by this; only a flatlined-at-0%-CPU
        // child is, after a generous grace period. Timing out returns `None`, exactly as a spawn
        // failure already does -- the caller's existing fallback (kill this ONE guest process with
        // `SIGSEGV`, which its own supervisor loop already respawns) is strictly better than an
        // unrecoverable, silent, whole-boot hang, and changes nothing for the overwhelmingly common
        // case where the child actually exits.
        const EXEC_COLLISION_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
        const EXEC_COLLISION_STALL_GRACE: std::time::Duration = std::time::Duration::from_secs(20);
        const EXEC_COLLISION_ABSOLUTE_CAP: std::time::Duration = std::time::Duration::from_secs(120);

        fn child_cpu_time_100ns(handle: windows_sys::Win32::Foundation::HANDLE) -> Option<u64> {
            let mut creation = windows_sys::Win32::Foundation::FILETIME::default();
            let mut exit = windows_sys::Win32::Foundation::FILETIME::default();
            let mut kernel = windows_sys::Win32::Foundation::FILETIME::default();
            let mut user = windows_sys::Win32::Foundation::FILETIME::default();
            let ok = unsafe {
                windows_sys::Win32::System::Threading::GetProcessTimes(
                    handle,
                    &raw mut creation,
                    &raw mut exit,
                    &raw mut kernel,
                    &raw mut user,
                )
            };
            if ok == 0 {
                return None;
            }
            let as_u64 = |ft: windows_sys::Win32::Foundation::FILETIME| -> u64 {
                (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
            };
            Some(as_u64(kernel) + as_u64(user))
        }

        let status = match cmd.spawn() {
            Ok(mut child) => {
                let handle = child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
                let start = std::time::Instant::now();
                let mut cpu_at_stall_start = child_cpu_time_100ns(handle);
                let mut stall_started = start;
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => break Ok(status),
                        Ok(None) => {}
                        Err(e) => break Err(e),
                    }
                    let elapsed = start.elapsed();
                    if elapsed >= EXEC_COLLISION_ABSOLUTE_CAP {
                        litebox_util_log::warn!(
                            path:% = path, elapsed:? = elapsed;
                            "spawn_exec_collision_child: replacement process exceeded the absolute \
                             time cap even while making CPU progress -- killing it rather than \
                             blocking this guest thread forever"
                        );
                        let _ = child.kill();
                        let _ = child.wait();
                        break Err(std::io::Error::other(
                            "spawn_exec_collision_child: absolute time cap exceeded",
                        ));
                    }
                    let cpu_now = child_cpu_time_100ns(handle);
                    // Same threshold and reasoning as `run_external_fault_watchdog_child`'s own
                    // `MEANINGFUL_CPU_DELTA_100NS`: a real tick of scheduler/measurement noise, not
                    // a claim of genuine work, so only a delta clearly above it resets the stall
                    // clock.
                    const MEANINGFUL_CPU_DELTA_100NS: u64 = 100_000;
                    let made_progress = match (cpu_at_stall_start, cpu_now) {
                        (Some(before), Some(after)) => {
                            after.saturating_sub(before) > MEANINGFUL_CPU_DELTA_100NS
                        }
                        _ => false,
                    };
                    if made_progress {
                        cpu_at_stall_start = cpu_now;
                        stall_started = std::time::Instant::now();
                    } else if stall_started.elapsed() >= EXEC_COLLISION_STALL_GRACE {
                        litebox_util_log::warn!(
                            path:% = path, stalled_for:? = stall_started.elapsed();
                            "spawn_exec_collision_child: replacement process made no CPU progress \
                             for the whole grace period -- treating it as wedged (most likely \
                             blocked forever on a sibling guest process's AF_UNIX socket this \
                             genuinely separate OS process cannot reach) and killing it rather than \
                             blocking this guest thread forever"
                        );
                        let _ = child.kill();
                        let _ = child.wait();
                        break Err(std::io::Error::other(
                            "spawn_exec_collision_child: stalled with no CPU progress",
                        ));
                    }
                    std::thread::sleep(EXEC_COLLISION_POLL_INTERVAL);
                }
            }
            Err(e) => Err(e),
        };
        match status {
            Ok(status) => {
                // `ExitStatus::code()` is `None` only for a signal-terminated child on Unix --
                // never on Windows, where every process exit carries a plain numeric code (a
                // process killed the way `TerminateProcess`/an unhandled exception would still
                // reports SOME `u32` code, just not through this enum's signal-shaped variant at
                // all, since Windows has no such variant). `unwrap_or(-1)` is therefore dead code
                // on this platform, kept only because the method returns a plain `Option<i32>`
                // signature shared with every other platform that might implement it.
                let raw_status = status.code().unwrap_or(-1);
                // Best-effort, same as the export call itself: a child that crashed or never
                // reached its own exit path leaves nothing here, which is no worse than the
                // pre-existing "nothing carries over" behaviour, not a new failure mode.
                let exported_writable_layer = match std::fs::read(&export_path) {
                    Ok(bytes) => {
                        litebox_util_log::warn!(
                            path:% = path, bytes:% = bytes.len();
                            "spawn_exec_collision_child: read back the child's exported writable layer"
                        );
                        Some(bytes)
                    }
                    Err(e) => {
                        litebox_util_log::warn!(
                            path:% = path, export_path:? = export_path, error:% = e;
                            "spawn_exec_collision_child: no exported writable layer to read back \
                             (the child may not have reached its own exit path)"
                        );
                        None
                    }
                };
                // Publish the child's export as the boot tree's new canonical "latest" snapshot
                // (see `CONTAINER_FS_SNAPSHOT_ENV_VAR`'s own doc comment) rather than discarding
                // it -- a `rename`, so this also takes care of removing `export_path` itself.
                // Only when the read above actually found something: `export_path` not existing
                // at all (the child never reached its own exit path) is the common, already-
                // logged case, not something to also report as a publish failure.
                if exported_writable_layer.is_some() {
                    let _ = process_fork::publish_as_container_fs_snapshot(export_path);
                }
                Some(litebox::platform::ExecCollisionChildResult {
                    raw_status,
                    exported_writable_layer,
                })
            }
            Err(e) => {
                // Covers both a genuine spawn failure (nothing above logged yet -- this is the
                // first and only line) and the stall/absolute-cap kill paths above (which already
                // logged their own specific reason; this is a short, generic follow-up, not a
                // duplicate diagnosis).
                litebox_util_log::warn!(
                    path:% = path, error:% = e;
                    "spawn_exec_collision_child: the replacement process did not exit normally -- \
                     falling back to killing the guest with SIGSEGV, matching a real execve() \
                     failure"
                );
                None
            }
        }
    }
}

/// Release the parent-side ends of pipes built for a cross-process `fork()` child that never
/// started. Consumes the bridges too, which shuts each parent-side end down and notifies its peer
/// -- the correct outcome for a child that will never transfer anything.
fn close_unused_pipe_ends(pumps: std::vec::Vec<(usize, litebox::platform::ForkPipeBridge)>) {
    for (local, _bridge) in pumps {
        // Safety: each handle came from `create_inheritable_child_pipe` and, on this path, was
        // never handed to a pump thread, so this is its only close.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(
                local as windows_sys::Win32::Foundation::HANDLE,
            )
        };
    }
}

/// Bridge one cross-process `fork()` child's pipe fd to the parent's own in-memory pipe.
///
/// Without this the child's `write(3, ..)` would land in an OS pipe nobody drains, and its
/// `read(0, ..)` would wait on an OS pipe nobody fills. Which way the bytes travel is decided by
/// `bridge`; see [`litebox::platform::ForkPipeBridge`].
///
/// Detached rather than joined, because its lifetime is the CHILD's, not this `fork()` call's --
/// the parent returns from `fork()` immediately, as it must. `local` and `child_process` are
/// passed as `usize` because a Windows `HANDLE` is a raw pointer and therefore `!Send`; the values
/// are process-wide and thread-agnostic, so re-forming them here is sound.
fn spawn_fork_child_pipe_pump(
    local: usize,
    bridge: litebox::platform::ForkPipeBridge,
    child_process: usize,
) {
    // Same defensive stack-size fix as the CHILD-side pipe pump this one mirrors
    // (`litebox_runner_linux_on_windows_userland`'s own pump thread, fixed the same pass after a
    // live-reproduced `STATUS_STACK_OVERFLOW` on a pipe-carrying cross-process fork under
    // concurrent host load) -- this parent-side half does plainer I/O (no shim/`Task` machinery
    // at all, just `ForkPipeBridge::read`/`write` over a raw Windows pipe handle) so it is less
    // likely to be the actual overflow site, but giving it the same generous, already-established
    // `GUEST_THREAD_STACK_SIZE` headroom costs nothing and keeps both halves of this one bridge
    // consistent rather than leaving one fixed and its sibling still on a bare 1 MiB default.
    const GUEST_THREAD_STACK_SIZE: usize = 32 * 1024 * 1024;
    std::thread::Builder::new()
        .stack_size(GUEST_THREAD_STACK_SIZE)
        .spawn(move || {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};

        match bridge {
            // The child writes. Drain the OS pipe into the parent's pipe until end of stream, then
            // drop the bridge: it holds the last non-descriptor reference to the parent-side write
            // end, so dropping it shuts that end down and `HUP`s the reader -- delivering EOF to
            // the guest exactly as a last `close()` would.
            litebox::platform::ForkPipeBridge::Sink(mut end) => {
                let mut buf = [0u8; 4096];
                loop {
                    let read = process_fork::read_from_inherited_handle(local, &mut buf);
                    if read == 0 {
                        litebox_util_log::debug!(
                            handle:% = local, owners:% = end.owners();
                            "fork-child pipe pump (parent): end of stream, releasing the guest pipe's write end so the guest's reader sees EOF"
                        );
                        break;
                    }
                    // Short writes are ordinary on a pipe, so loop until the chunk is placed.
                    // `None` means the parent-side pipe is gone (the guest closed its reader),
                    // which makes everything still in flight undeliverable -- stop rather than
                    // spin.
                    let mut off = 0usize;
                    while off < read {
                        match end.write(&buf[off..read]) {
                            Some(0) | None => break,
                            Some(n) => off += n,
                        }
                    }
                }
                drop(end);
            }
            // The child reads. WAIT FIRST: on a real fork parent and child share one byte stream,
            // and draining eagerly here would steal bytes from a reader still live in this
            // process. `owners() == 1` says this bridge is the end's sole owner, i.e. the guest
            // parent has closed its own descriptor and there is no such reader left -- which is
            // exactly what a shell does immediately after forking a pipeline stage.
            //
            // A parent that never closes its copy is the genuinely-shared case, which no bridge
            // built out of a second OS pipe can reproduce; there the child's inherited fd simply
            // never yields, and this thread ends when the child does rather than waiting for ever.
            litebox::platform::ForkPipeBridge::Source(mut end) => {
                const POLL: core::time::Duration = core::time::Duration::from_millis(2);
                while end.owners() > 1 {
                    if process_fork::process_has_exited(child_process) {
                        litebox_util_log::debug!(
                            handle:% = local;
                            "fork-child pipe pump (parent): child exited while the guest still held its own copy of this pipe's read end; nothing was forwarded"
                        );
                        // Safety: sole owner; closed exactly once, here.
                        unsafe { CloseHandle(local as HANDLE) };
                        return;
                    }
                    std::thread::sleep(POLL);
                }
                let mut buf = [0u8; 4096];
                loop {
                    match end.read(&mut buf) {
                        Some(0) | None => {
                            litebox_util_log::debug!(
                                handle:% = local;
                                "fork-child pipe pump (parent): guest pipe reached EOF, closing the child's inherited read end"
                            );
                            break;
                        }
                        Some(n) => {
                            if !process_fork::write_all_to_inherited_handle(local, &buf[..n]) {
                                // The child is gone or has closed its end; nothing left to deliver.
                                break;
                            }
                        }
                    }
                }
                drop(end);
            }
        }
        // Safety: this thread is the sole owner of `local`, and closes it exactly once.
        unsafe { CloseHandle(local as HANDLE) };
    })
        .expect("failed to spawn cross-process fork parent's pipe pump thread");
}

impl litebox::platform::SystemInfoProvider for WindowsUserland {
    fn get_syscall_entry_point(&self) -> usize {
        syscall_callback as *const () as usize
    }

    fn get_vdso_address(&self) -> Option<usize> {
        // Windows doesn't have VDSO equivalent, return None
        None
    }

    fn env_flag(&self, name: &str) -> bool {
        // Cached per thread, because callers reasonably treat a trait method named `env_flag` as
        // a predicate cheap enough to consult from a hot path -- the shim's syscall dispatch did
        // exactly that, twice per syscall. On Windows it is not cheap: every call enters ntdll's
        // process-wide environment critical section (`RtlQueryEnvironmentVariable`) and allocates
        // twice. Across a multi-threaded guest that makes one OS-owned lock the busiest lock in
        // the process, and it widens the window in which `ThreadHandle::interrupt` can suspend a
        // thread that is holding it (see `diag_interrupt_enabled` for the deadlock that caused).
        //
        // Per-thread rather than process-wide on purpose: the cache is then read and written with
        // no synchronisation at all, so a fix for lock contention cannot reintroduce any. Host
        // environment variables do not change during a run, so the duplication is free.
        thread_local! {
            static CACHE: RefCell<std::vec::Vec<(std::string::String, bool)>> =
                const { RefCell::new(std::vec::Vec::new()) };
        }
        CACHE.with(|c| {
            let hit = c.borrow().iter().find(|(k, _)| k == name).map(|&(_, v)| v);
            if let Some(v) = hit {
                return v;
            }
            let v = std::env::var_os(name).is_some_and(|v| !v.is_empty());
            c.borrow_mut().push((name.into(), v));
            v
        })
    }

    fn env_value(&self, name: &str) -> Option<std::string::String> {
        // Cached per thread for exactly the reasons `env_flag` above documents at length -- the
        // cost being avoided is ntdll's process-wide environment critical section, which is a
        // property of the lookup itself and not of what the lookup returns. A separate cache
        // rather than a shared one because the two answer different questions ("is it set" vs
        // "what is it"), and conflating them would make the flag cache's `bool` lossy.
        thread_local! {
            static VALUE_CACHE: RefCell<
                std::vec::Vec<(std::string::String, Option<std::string::String>)>,
            > = const { RefCell::new(std::vec::Vec::new()) };
        }
        VALUE_CACHE.with(|c| {
            if let Some(v) = c.borrow().iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()) {
                return v;
            }
            let v = std::env::var_os(name)
                .and_then(|v| v.into_string().ok())
                .filter(|v| !v.is_empty());
            c.borrow_mut().push((name.into(), v.clone()));
            v
        })
    }

    fn cpu_count(&self) -> usize {
        let count = self.sys_info.read().unwrap().dwNumberOfProcessors;
        usize::try_from(count).unwrap_or(1).max(1)
    }

    /// Same `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `GetExitCodeProcess` liveness
    /// check `RawMutex::try_recover_from_dead_holder_unregistered` already uses for its own
    /// dead-holder recovery (see that function's doc comment) -- not factored into a shared helper
    /// this pass (that `RawMutex` method is a private inherent method on a different type in this
    /// same file, and unifying them is a pure refactor out of scope for the live bug this exists
    /// to fix), but deliberately the identical two Win32 calls and the identical dead/alive
    /// verdict rule, so a future unification is a mechanical dedup rather than a behavior change.
    fn is_process_alive(&self, pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        // SAFETY: liveness probe only, minimum access requested.
        let handle = unsafe {
            Win32_Threading::OpenProcess(Win32_Threading::PROCESS_QUERY_LIMITED_INFORMATION, 0, pid)
        };
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        // SAFETY: `handle` was just successfully opened above.
        let ok = unsafe { Win32_Threading::GetExitCodeProcess(handle, &raw mut exit_code) };
        // SAFETY: `handle` is a valid, owned handle not used again after this point.
        unsafe {
            Win32_Foundation::CloseHandle(handle);
        }
        ok != 0 && exit_code == STILL_ACTIVE
    }

    /// Real host memory via `GlobalMemoryStatusEx`, with the AVAILABLE figure deliberately
    /// discounted before it is reported to the guest.
    ///
    /// The discount is the point of this function, not an incidental detail. `ullAvailPhys` is
    /// what the host has free *right now*, shared with every other process on the machine
    /// (including this one's own rootfs page cache). Handing a guest that whole figure invites it
    /// to size a buffer pool from memory that is already spoken for -- see `memory_info_kb`'s doc
    /// comment for the measured Xorg case where advertising 3 GiB free produced 8.9 GiB peaks and
    /// repeated watchdog kills. Reporting half, capped at 2 GiB, keeps a guest's own sizing logic
    /// well inside what the host can actually satisfy while still being truthful in shape (it
    /// tracks real pressure: report less when the host genuinely has less).
    fn memory_info_kb(&self) -> (u64, u64) {
        let mut status = windows_sys::Win32::System::SystemInformation::MEMORYSTATUSEX {
            dwLength: u32::try_from(core::mem::size_of::<windows_sys::Win32::System::SystemInformation::MEMORYSTATUSEX>())
                .unwrap_or(64),
            ..Default::default()
        };
        // SAFETY: `status` is a correctly-sized, correctly-`dwLength`-tagged local, which is the
        // entire contract of `GlobalMemoryStatusEx`.
        let ok = unsafe { windows_sys::Win32::System::SystemInformation::GlobalMemoryStatusEx(&raw mut status) };
        if ok == 0 {
            // Fall back to the trait's conservative default rather than reporting anything
            // invented: a failed query is not a reason to tell the guest it has memory.
            return (1024 * 1024, 512 * 1024);
        }
        let total_kb = status.ullTotalPhys / 1024;
        /// Never advertise more than this much available memory, however much the host has free.
        /// A guest sizing a pool from a very large figure is the failure mode this whole function
        /// exists to prevent; past this point more headroom buys nothing real.
        const AVAIL_CEILING_KB: u64 = 2 * 1024 * 1024;
        let avail_kb = (status.ullAvailPhys / 1024 / 2).min(AVAIL_CEILING_KB);
        (total_kb.max(1), avail_kb.max(64 * 1024))
    }
}

thread_local! {
    // Use `ManuallyDrop` for more efficient TLS accesses, since this is always
    // dropped manually before the thread exits.
    static PLATFORM_TLS: Cell<*mut ()> = const { Cell::new(core::ptr::null_mut()) };
}

/// WindowsUserland platform's thread-local storage implementation.
unsafe impl litebox::platform::ThreadLocalStorageProvider for WindowsUserland {
    fn get_thread_local_storage() -> *mut () {
        PLATFORM_TLS.get()
    }

    unsafe fn replace_thread_local_storage(new_tls: *mut ()) -> *mut () {
        PLATFORM_TLS.replace(new_tls)
    }
}

impl litebox::platform::CrngProvider for WindowsUserland {
    fn fill_bytes_crng(&self, buf: &mut [u8]) {
        getrandom::fill(buf).expect("getrandom failed");
    }
}

/// Dummy `VmemPageFaultHandler`.
///
/// Page faults are handled transparently by the host Windows kernel.
/// Provided to satisfy trait bounds for `PageManager::handle_page_fault`.
impl litebox::mm::linux::VmemPageFaultHandler for WindowsUserland {
    unsafe fn handle_page_fault(
        &self,
        _fault_addr: usize,
        _flags: litebox::mm::linux::VmFlags,
        _error_code: u64,
    ) -> Result<(), litebox::mm::linux::PageFaultError> {
        unreachable!("host kernel handles page faults for Windows userland")
    }

    fn access_error(_error_code: u64, _flags: litebox::mm::linux::VmFlags) -> bool {
        unreachable!("host kernel handles page faults for Windows userland")
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::AtomicU32;
    use std::thread::sleep;

    use crate::WindowsUserland;
    use crate::process_memory_range_by_regions;
    use litebox::platform::PageManagementProvider;
    use litebox::platform::RawConstPointer;
    use litebox::platform::RawMutex;
    use litebox::platform::page_mgmt::FixedAddressBehavior;
    use litebox::platform::page_mgmt::MemoryRegionPermissions;

    /// `write_crash_minidump` must actually produce a readable dump, not merely compile.
    ///
    /// Exercised with a null `EXCEPTION_POINTERS`, which is the one part of a real fatal fault that
    /// cannot be staged from a test: `MiniDumpWriteDump` treats a dump with no exception record as
    /// valid (it just has no faulting-thread annotation), so everything else this function does --
    /// building the path without allocating, creating the file, and getting the OS to walk every
    /// thread -- is exercised for real against the real API.
    ///
    /// Worth having as a test rather than trusting the fatal path: that path runs once, on a dying
    /// process, where a silent failure would leave exactly the no-evidence situation this function
    /// exists to end. A wrong `CreateFileW` flag or a mis-built path would be invisible there.
    #[test]
    fn crash_minidump_writes_a_real_dump_file() {
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("litebox-crash-{pid}.dmp"));
        // A stale dump from an earlier run of this test would make the assertion below vacuous.
        let _ = std::fs::remove_file(&path);

        super::write_crash_minidump(core::ptr::null_mut());

        let meta = std::fs::metadata(&path)
            .unwrap_or_else(|e| panic!("no dump at {}: {e}", path.display()));
        // A minidump of a live multi-threaded process is never tiny; a few hundred bytes would mean
        // the header was written and the thread walk failed.
        assert!(
            meta.len() > 4096,
            "dump at {} is only {} bytes, which means MiniDumpWriteDump did not really run",
            path.display(),
            meta.len()
        );
        // `MDMP` is the minidump signature, and checking it is what distinguishes a real dump from
        // any file of the right size.
        let head = std::fs::read(&path).expect("dump is readable");
        assert_eq!(&head[..4], b"MDMP", "file is not a minidump");

        let _ = std::fs::remove_file(&path);
    }

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
        let platform = WindowsUserland::new();
        let reserved_pages: Vec<_> =
            <WindowsUserland as PageManagementProvider<4096>>::reserved_pages(platform).collect();

        // Check that the reserved pages are not empty
        assert!(!reserved_pages.is_empty(), "No reserved pages found");

        // Check that the reserved pages are in order and non-overlapping
        let mut prev = 0;
        for page in reserved_pages {
            assert!(page.start >= prev);
            assert!(page.end > page.start);
            prev = page.end;
        }
    }

    #[test]
    fn test_page_provider() {
        let collect_regions = |r| {
            let mut regions = Vec::new();
            process_memory_range_by_regions(
                r,
                |region, state| -> Result<bool, core::convert::Infallible> {
                    regions.push((region, state));
                    Ok(true)
                },
            )
            .unwrap();
            regions
        };

        let platform = WindowsUserland::new();
        let system_allocation_granularity =
            platform.sys_info.read().unwrap().dwAllocationGranularity as usize;
        // Allocate some pages: it should reserve `system_allocation_granularity` bytes but only commit 0x1000 bytes
        let addr = <WindowsUserland as PageManagementProvider<4096>>::allocate_pages(
            platform,
            0..0x1000,
            MemoryRegionPermissions::WRITE,
            false,
            true,
            FixedAddressBehavior::Hint,
        )
        .unwrap()
        .as_usize();
        assert_eq!(
            collect_regions(addr..addr + system_allocation_granularity),
            vec![
                (
                    addr..addr + 0x1000,
                    windows_sys::Win32::System::Memory::MEM_COMMIT
                ),
                (
                    addr + 0x1000..addr + system_allocation_granularity,
                    windows_sys::Win32::System::Memory::MEM_RESERVE
                ),
            ]
        );

        assert!(system_allocation_granularity >= 0x1_0000);
        // We should be able to allocate [addr + 0x8000, addr + 0x1_0000)
        let addr2 = <WindowsUserland as PageManagementProvider<4096>>::allocate_pages(
            platform,
            (addr + 0x8000)..(addr + 0x1_0000),
            MemoryRegionPermissions::WRITE,
            false,
            true,
            FixedAddressBehavior::Hint,
        )
        .unwrap()
        .as_usize();
        // Even though `fixed_address` is false, we should still get the requested address if it's free.
        assert_eq!(addr2, addr + 0x8000);
        assert_eq!(
            collect_regions(addr..addr + 0x1_0000),
            vec![
                (
                    addr..addr + 0x1000,
                    windows_sys::Win32::System::Memory::MEM_COMMIT
                ),
                (
                    addr + 0x1000..addr + 0x8000,
                    windows_sys::Win32::System::Memory::MEM_RESERVE
                ),
                (
                    addr + 0x8000..addr + 0x1_0000,
                    windows_sys::Win32::System::Memory::MEM_COMMIT
                ),
            ]
        );

        // Try to allocate [addr + 0x4000, addr + 0x1_0000), which overlaps with existing committed pages.
        // OS should allocate a new region instead of the requested one (as `fixed_address` is false)
        let addr3 = <WindowsUserland as PageManagementProvider<4096>>::allocate_pages(
            platform,
            (addr + 0x4000)..(addr + 0x1_0000),
            MemoryRegionPermissions::WRITE,
            false,
            true,
            FixedAddressBehavior::Hint,
        )
        .unwrap()
        .as_usize();
        assert_ne!(addr3, addr + 0x4000);
    }

    /// Regression coverage for the `node -e "..."` hang: [`super::stdin_ready_raw_handle`] must
    /// give a genuine non-blocking readiness answer instead of the old hardcoded
    /// `EpollDescriptor::File`'s-caller-side "stdin is always readable" assumption that let
    /// libuv's poll-then-read pattern land in [`super::read_from_raw_handle`]'s blocking
    /// `ReadFile` with nothing queued. This drives `STD_INPUT_HANDLE` through both non-console
    /// backings the function distinguishes (`FILE_TYPE_PIPE`/`FILE_TYPE_UNKNOWN`) via real OS
    /// handles, since a genuinely console-backed `STD_INPUT_HANDLE` is not available in this
    /// test-runner's (non-interactive) process.
    #[test]
    fn test_stdin_ready_pipe_and_regular_file() {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::Storage::FileSystem::WriteFile;
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, SetStdHandle};
        use windows_sys::Win32::System::Pipes::CreatePipe;

        // Swap `STD_INPUT_HANDLE` for the lifetime of this test and always restore it, so this
        // doesn't corrupt any other test's view of the process's real stdin.
        struct RestoreStdin(HANDLE);
        impl Drop for RestoreStdin {
            fn drop(&mut self) {
                unsafe {
                    SetStdHandle(STD_INPUT_HANDLE, self.0);
                }
            }
        }
        let _restore = RestoreStdin(unsafe { GetStdHandle(STD_INPUT_HANDLE) });

        // `stdin_ready_raw_handle` only needs a live `WindowsUserland` instance for its
        // `FILE_TYPE_CHAR` (real console) branch, which this test deliberately does not exercise
        // (see this test's doc comment) -- but the parameter is required regardless, so get a real
        // instance the same way any other caller would.
        let platform = WindowsUserland::new();

        // An empty anonymous pipe (`FILE_TYPE_PIPE`) with nothing written yet: must report
        // not-ready, since a `ReadFile` on it would block until a writer sends data -- this is
        // the exact "poll says ready, read blocks forever" hazard this function exists to avoid.
        let (mut read_handle, mut write_handle): (HANDLE, HANDLE) =
            (core::ptr::null_mut(), core::ptr::null_mut());
        let ok = unsafe {
            CreatePipe(
                &raw mut read_handle,
                &raw mut write_handle,
                core::ptr::null(),
                0,
            )
        };
        assert_ne!(ok, 0, "CreatePipe failed: {}", unsafe {
            windows_sys::Win32::Foundation::GetLastError()
        });
        unsafe {
            SetStdHandle(STD_INPUT_HANDLE, read_handle);
        }
        assert!(
            !super::stdin_ready_raw_handle(platform),
            "an empty pipe with no writer output yet must not report ready"
        );

        // Write a byte into the pipe: now a `ReadFile` would return immediately, so readiness
        // must flip to `true`.
        let mut written: u32 = 0;
        let ok = unsafe {
            WriteFile(
                write_handle,
                [7u8].as_ptr(),
                1,
                &raw mut written,
                core::ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0);
        assert!(
            super::stdin_ready_raw_handle(platform),
            "a pipe with data already written must report ready"
        );

        unsafe {
            CloseHandle(read_handle);
            CloseHandle(write_handle);
        }

        // A null/invalid handle (no stdin attached at all) must report ready: `read_from_stdin`
        // treats this as already-closed/EOF, which is an immediate (non-blocking) outcome.
        unsafe {
            SetStdHandle(STD_INPUT_HANDLE, core::ptr::null_mut());
        }
        assert!(
            super::stdin_ready_raw_handle(platform),
            "no stdin handle attached at all must report ready (matches EOF semantics)"
        );
    }
}
