// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! A [LiteBox platform](../litebox/platform/index.html) for running LiteBox on userland Windows.

// Restrict this crate to only work on Windows. For now, we are restricting this to only x86-64
// Windows, but we _may_ allow for more in the future, if we find it useful to do so.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

mod ctxwatch;
mod fork_verify;
mod net;

use core::cell::Cell;
use core::panic;
use core::sync::atomic::{AtomicU32, Ordering};
use core::time::Duration;
use std::cell::RefCell;
use std::os::raw::c_void;
use std::os::windows::io::AsRawHandle as _;
use std::sync::{Arc, Mutex, OnceLock};

use litebox::platform::ImmediatelyWokenUp;
use litebox::platform::UnblockedOrTimedOut;
use litebox::platform::page_mgmt::{
    AllocationError, FixedAddressBehavior, MemoryRegionPermissions, SharedMemoryError,
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
    System::Memory::{
        self as Win32_Memory, CreateFileMappingW, MapViewOfFile3, PrefetchVirtualMemory,
        UnmapViewOfFileEx, VirtualAlloc2, VirtualFree, VirtualProtect,
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
}

/// Diagnostic tracing gated by `LITEBOX_VEH_TRACE=1`, added to root-cause the intermittent
/// hang/crash in the `apk add nodejs` repro. Temporary; remove once root-caused.
///
/// Deliberately re-reads the environment on every call rather than caching the result behind a
/// `static` (this crate's bare-static count is tracked by `dev_tests/src/ratchet.rs`'s
/// `ratchet_globals`, which is actively trying to reduce, not grow, that count): this is only
/// called on already-rare exception-handling paths, so the cost of an uncached env lookup is
/// negligible relative to introducing another global.
pub(crate) fn veh_trace_enabled() -> bool {
    std::env::var_os("LITEBOX_VEH_TRACE").is_some()
}

unsafe extern "system" fn vectored_exception_handler(
    exception_info: *mut EXCEPTION_POINTERS,
) -> i32 {
    let Some(tls) = get_tls_ptr() else {
        // TLS slot not initialized yet; cannot be in guest
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let tls = unsafe { &*tls };
    let (info, exception_record, context);
    unsafe {
        info = *exception_info;
        exception_record = &*info.ExceptionRecord;
        context = &mut *info.ContextRecord;
    }

    if veh_trace_enabled() {
        unsafe extern "C" {
            safe static __ImageBase: c_void;
        }
        let image_base = (&raw const __ImageBase).addr();
        eprintln!(
            "[veh] tid={:?} code={:#x} rip={:#x} rva={:#x} addr={:#x} is_in_guest={} is_verifying={} rdfsbase={:#x} thread_fs_base={:#x}",
            std::thread::current().id(),
            exception_record.ExceptionCode,
            context.Rip,
            {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "diagnostic-only; this platform is x86_64-only, rip fits in usize"
                )]
                (context.Rip as usize).wrapping_sub(image_base)
            },
            exception_record.ExceptionInformation[1],
            tls.is_in_guest.get(),
            fork_verify::is_verifying(tls),
            unsafe { litebox_common_linux::rdfsbase() },
            WindowsUserland::get_thread_fs_base(),
        );
        if exception_record.ExceptionCode == 0xC000_0096_u32.cast_signed() {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "diagnostic-only; this platform is x86_64-only, rip fits in usize"
            )]
            let rip = context.Rip as usize;
            let mut buf = [0u8; 16];
            let n = fork_verify::read_code_bytes_for_diagnostics(rip, &mut buf);
            eprintln!("[veh] rip bytes ({n}): {:02x?}", &buf[..n]);
            let mut before = [0u8; 8];
            let before_start = rip.wrapping_sub(8);
            let nb = fork_verify::read_code_bytes_for_diagnostics(before_start, &mut before);
            eprintln!("[veh] rip-8 bytes ({nb}): {:02x?}", &before[..nb]);
            fork_verify::describe_crash_page_for_diagnostics(rip);
            eprintln!(
                "[veh] crash regs rdi={:#x} rsi={:#x} rdx={:#x} rax={:#x} rsp={:#x} rbp={:#x}",
                context.Rdi, context.Rsi, context.Rdx, context.Rax, context.Rsp, context.Rbp,
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
            && context.Rip != 0
        {
            let saved = WindowsUserland::get_thread_fs_base();
            if saved != 0 {
                if veh_trace_enabled() {
                    eprintln!(
                        "[veh] tid={:?} host-mode FS_BASE-reset in-place repair (rip={:#x})",
                        std::thread::current().id(),
                        context.Rip,
                    );
                }
                unsafe { litebox_common_linux::wrfsbase(saved) };
                return EXCEPTION_CONTINUE_EXECUTION;
            }
        }

        // This might be a faulting guest memory access in LiteBox code. Try to
        // recover.
        if exception_record.ExceptionCode == Win32_Foundation::EXCEPTION_ACCESS_VIOLATION
            && let Some(recover) =
                litebox::mm::exception_table::search_exception_tables(context.Rip.trunc())
        {
            // Found a matching exception table entry.
            context.Rip = recover as u64;
            return EXCEPTION_CONTINUE_EXECUTION;
        } else {
            // Not one of our exceptions; let other handlers process it.
            return EXCEPTION_CONTINUE_SEARCH;
        }
    }

    // Windows clears this thread's FS_BASE MSR back to 0 on its own initiative, apparently as
    // part of ordinary scheduling (observed to recur many times per second under load, e.g.
    // during `apk add nodejs`'s guest dynamic-linking/TLS-heavy startup). A guest `mov %fs:...`
    // hit while FS_BASE is 0 reads/writes through linear address `0 + offset` instead of the
    // real TLS block, which is (almost always) unmapped and therefore an ordinary `#PF` here,
    // reported as `EXCEPTION_ACCESS_VIOLATION` -- indistinguishable, without this check, from a
    // genuine guest segfault.
    //
    // Detect and repair this *before* any other exception-code-specific handling (in particular
    // before the `EXCEPTION_SINGLE_STEP` triage below, which hands off to
    // `fork_verify::on_single_step` -- that function has no notion of FS_BASE at all, and running
    // its source/destination-range and instruction-decode logic against a thread whose FS_BASE is
    // transiently wrong would either misclassify the trap or simply waste the step; simplest and
    // safest is to make sure FS_BASE is never wrong by the time exception-code-specific logic
    // runs, for every exception code, not just the ones this repro happens to hit).
    //
    // Repair happens in place, without ever leaving guest mode: just `wrfsbase` the stored value
    // back and retry the exact same faulting instruction via `EXCEPTION_CONTINUE_EXECUTION`. This
    // used to instead route through `interrupt_callback` (`set_context_to_interrupt_callback`),
    // which is far more expensive -- it leaves guest mode, saves the full guest context, and takes
    // a `NtContinue` round-trip through host Rust code before `switch_to_guest` gets back around
    // to restoring FS_BASE and re-entering the guest. Under the same scheduler pressure that
    // causes FS_BASE to be cleared in the first place, that round-trip reliably took long enough
    // for FS_BASE to be cleared *again* before the guest completed even one more instruction,
    // producing an unbounded livelock: thousands of these access violations in a row, forward
    // progress permanently stalled, observed in practice as the reported indefinite hang (see the
    // `LITEBOX_VEH_TRACE=1` diagnostic traces this fix was root-caused from -- runs that hung
    // showed exactly this pattern: repeated `EXCEPTION_ACCESS_VIOLATION` with `rdfsbase() == 0` at
    // a different `rip` each time, `is_verifying == false`, never reaching a third occurrence of
    // the same instruction because the guest one instruction at a time). Fixing FS_BASE directly
    // in the handler removes every one of those host round-trip's kernel transitions from the
    // recovery path, so recovery is a single MSR write plus a `CONTINUE_EXECUTION` return -- no
    // syscalls, no context save, no scheduling-visible event of its own to compound the problem.
    //
    // This does forgo the old comment's stated rationale for going through `interrupt_callback`
    // ("avoid missing a real interrupt that arrives while resuming the guest"): a pending
    // interrupt is not inspected here before resuming. This is safe: interrupt/signal delivery to
    // a running guest is already only ever "eventually", never guaranteed at a specific
    // instruction boundary (the same is true on real hardware), and `ThreadHandle::interrupt`
    // does not depend on this path at all -- it suspends the target thread directly and rewrites
    // its context itself, which still works correctly regardless of whether this handler happens
    // to run in between. A real interrupt is caught at the next point that already checks for one
    // (the next syscall, or the next time this same thread is suspended-and-inspected by
    // `ThreadHandle::interrupt`), exactly as it would be if this exact access violation had not
    // happened to occur at all.
    if exception_record.ExceptionCode == Win32_Foundation::EXCEPTION_ACCESS_VIOLATION
        && unsafe { litebox_common_linux::rdfsbase() } == 0
        // See the matching guard in the `!is_in_guest` branch above: `Rip == 0` means this is not
        // a genuine FS_BASE-reset fault at a real instruction, and blindly repairing-and-resuming
        // would just re-fault at address 0 forever. Fall through to the normal exception path
        // below (single-step triage / `exception_callback`) instead of looping silently.
        && context.Rip != 0
    {
        let saved = WindowsUserland::get_thread_fs_base();
        if saved != 0 {
            if veh_trace_enabled() {
                eprintln!(
                    "[veh] tid={:?} FS_BASE-reset in-place repair (rip={:#x})",
                    std::thread::current().id(),
                    context.Rip,
                );
            }
            unsafe { litebox_common_linux::wrfsbase(saved) };
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
    // FS_BASE-reset repair applies here too, and matters *far* more here than on the plain
    // (non-single-stepped) guest-execution path above: single-stepping means every guest
    // instruction is its own kernel round-trip through this handler, which is exactly the kind of
    // scheduling-visible event the FS_BASE-reset behavior above is already keyed off of ("observed
    // to recur many times per second under load") -- so a `fork()` child under verification hits
    // the reset on very nearly every single instruction (confirmed via `LITEBOX_VEH_TRACE=1`:
    // >99% of `on_single_step` calls during a real `apk add nodejs` run observed `rdfsbase() ==
    // 0`), not merely "many times per second". Before this fix, this path had no FS_BASE repair of
    // its own: `on_single_step` only *logged* the corruption and proceeded with its rip/instruction
    // classification regardless (which is safe -- it never reads `%fs:`-relative memory itself,
    // only CPU registers and instruction bytes at `rip`), then re-armed `TF` and resumed the
    // *original* guest instruction with FS_BASE still zero. If that instruction touched `%fs:`, it
    // then took a *second* trap -- `EXCEPTION_ACCESS_VIOLATION` this time -- which the repair above
    // fixes and retries via `CONTINUE_EXECUTION`, but with `TF` still armed the whole time, so the
    // very next instruction immediately single-steps again, and if FS_BASE has already been reset
    // yet again by then (observed to be the common case), the two traps alternate in an extremely
    // tight loop -- thousands of round trips to make a handful of instructions of real forward
    // progress, exactly the "quadratic-ish" slowdown reported as an apparent hang. Repairing FS_BASE
    // in place here, before `on_single_step` runs, means the guest instruction that resumes after
    // this step always sees correct FS_BASE the first time, so it never needs that second
    // access-violation-and-retry round trip at all: one MSR rewrite replaces two full VEH
    // dispatches.
    if unsafe { litebox_common_linux::rdfsbase() } == 0 {
        let saved = WindowsUserland::get_thread_fs_base();
        if saved != 0 {
            unsafe { litebox_common_linux::wrfsbase(saved) };
        }
    }

    let mut synthesized_record = None;
    if exception_record.ExceptionCode == Win32_Foundation::EXCEPTION_SINGLE_STEP {
        match fork_verify::on_single_step(tls, context) {
            fork_verify::StepOutcome::Continue => return EXCEPTION_CONTINUE_EXECUTION,
            fork_verify::StepOutcome::StalePointer { address, is_write } => {
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
    let exception_record_ptr = tls
        .host_sp
        .get()
        .cast::<EXCEPTION_RECORD>()
        .wrapping_byte_sub(EXCEPTION_RECORD_RESERVE);
    assert!(exception_record_ptr.is_aligned());
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

        let reserved_pages = Self::read_memory_maps();

        let platform = Self {
            reserved_pages,
            sys_info: std::sync::RwLock::new(sys_info),
            net_gateway: std::sync::OnceLock::new(),
            console_stdin_reader: std::sync::OnceLock::new(),
        };

        // Initialize it's own fs-base (for the main thread)
        WindowsUserland::init_thread_fs_base();

        // Windows sets FS_BASE to 0 regularly upon scheduling; we register an exception handler
        // to set FS_BASE back to a "stored" value whenever we notice that it has become 0.
        unsafe {
            let _ = AddVectoredExceptionHandler(0, Some(vectored_exception_handler));
        }

        // Register a console control handler to receive Ctrl+C / Ctrl+Break
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
                Some(ctrl_c_handler),
                1, // TRUE — add the handler
            );
        }

        // Watch for real console window resizes and deliver SIGWINCH. There is no Win32 resize
        // *event* callback equivalent to `SetConsoleCtrlHandler` -- `GetConsoleScreenBufferInfo`
        // polling on a dedicated thread is the standard approach (e.g. used by libuv/Node's own
        // Windows tty backend). This deliberately does not touch `STD_INPUT_HANDLE` or the input
        // event queue at all (unlike `ConsoleStdinReader`), so it cannot race with or steal
        // events from the existing stdin reader thread -- `GetConsoleScreenBufferInfo` reads the
        // *output* buffer's window-size state, a wholly separate API surface.
        std::thread::Builder::new()
            .name("litebox-console-resize-watcher".to_owned())
            .spawn(console_resize_watcher_thread_body)
            .expect("failed to spawn console resize watcher thread");

        Box::leak(Box::new(platform))
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

    fn read_memory_maps() -> alloc::vec::Vec<core::ops::Range<usize>> {
        let mut reserved_pages = alloc::vec::Vec::new();
        let mut address = 0usize;

        loop {
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
                reserved_pages.push(core::ops::Range {
                    start: mbi.BaseAddress as usize,
                    end: (mbi.BaseAddress as usize + mbi.RegionSize),
                });
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
        // TODO: Currently we are using a static thread ID and credentials (faked).
        // This is a placeholder for future implementation to use passthrough.
        //
        // Credentials are root (uid/gid 0), matching a real container's initial process (a
        // fresh OCI/container rootfs such as Alpine ships `/`, `/etc`, `/lib`, etc. root-owned
        // at mode 0755, and its init process runs as root absent an explicit `USER` directive).
        // Callers that build the guest's file system (e.g.
        // `litebox_runner_linux_on_windows_userland`) must set the in-memory file system's
        // persistent user to match via `litebox::fs::in_mem::FileSystem::set_default_user`, or
        // `getuid()` will disagree with what the filesystem layer's permission checks enforce.
        litebox_common_linux::TaskParams {
            pid: 1000,
            // TODO: placeholder for actual PPID
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
    run_thread_inner(&shim, ctx);
}

fn run_thread_inner(
    shim: &dyn litebox::shim::EnterShim<ExecutionContext = litebox_common_linux::PtRegs>,
    ctx: &mut litebox_common_linux::PtRegs,
) {
    let tls_state = TlsState::new();
    tls_state
        .guest_context_top
        .set(std::ptr::from_mut(ctx).wrapping_add(1));

    let mut thread_ctx = ThreadContext {
        shim,
        ctx,
        tls: &tls_state,
    };
    ThreadHandle::run_with_handle(&tls_state, || unsafe {
        run_thread_arch(&mut thread_ctx, &tls_state);
    });
}

static TLS_INDEX: AtomicU32 = AtomicU32::new(u32::MAX);

struct TlsState {
    host_sp: Cell<*mut u128>,
    host_bp: Cell<*mut u128>,
    guest_context_top: Cell<*mut litebox_common_linux::PtRegs>,
    scratch: Cell<usize>,
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
    /// The `(effective address, loaded value)` of the most recent explicit-memory-operand read
    /// `fork_verify::on_single_step` observed, from the immediately preceding single-step trap on
    /// this thread -- `None` if the preceding step had no explicit memory operand, or was not
    /// itself observed under verification.
    ///
    /// Lets [`fork_verify::on_single_step`] recognize a register-indirect `call reg`/`jmp reg`
    /// whose target was loaded from a stale memory slot one instruction earlier (`mov reg,
    /// [slot]` then `call reg`), so that slot can be healed even though the call/jmp instruction
    /// itself has no memory operand to read the slot address from directly. See that function's
    /// case (4) for the full reasoning, including why this is restricted to the *immediately
    /// preceding* step's read rather than an unbounded history (a false match against a stale,
    /// unrelated earlier read would risk the same false-positive hazard case (3)'s doc comment
    /// describes).
    fork_verify_last_load: Cell<Option<(usize, usize)>>,
    /// Backing state for the diagnostic code-page watchpoint (`LITEBOX_CODEWATCH=1`); see
    /// [`fork_verify`]'s `codewatch` module. A field here rather than a bare `static`, matching
    /// `WindowsUserland::console_stdin_reader`'s reasoning -- it keeps this diagnostic off the
    /// crate's ratcheted bare-static count, and per-thread is its natural scope anyway (the
    /// `fork()` child arms the ranges on its own thread and is the thread that traps on them).
    codewatch: fork_verify::CodewatchState,
}

/// Scratch space (in bytes) reserved below `host_sp` for the `EXCEPTION_RECORD` that
/// `vectored_exception_handler` writes when redirecting to `exception_callback`. Must be
/// large enough to hold a full `EXCEPTION_RECORD` (152 bytes on x86_64) plus alignment slack,
/// and must keep clear of `[host_sp]`/`[host_sp + 8]`, where `run_thread_arch`'s prologue
/// pushes `thread_ctx` -- `exception_callback` (like `syscall_callback` and
/// `interrupt_callback`) reads `thread_ctx` back via `[rsp]`, so `Rsp` is always set to
/// `host_sp` itself, unmodified; the exception record lives in this separate reserve instead
/// of overlapping the `Rsp` landing spot.
const EXCEPTION_RECORD_RESERVE: usize = 4096;

impl TlsState {
    /// Creates a new `TlsState` with all fields zeroed / defaulted.
    fn new() -> Self {
        Self {
            host_sp: Cell::new(core::ptr::null_mut()),
            host_bp: Cell::new(core::ptr::null_mut()),
            guest_context_top: core::ptr::null_mut::<litebox_common_linux::PtRegs>().into(),
            scratch: 0.into(),
            is_in_guest: false.into(),
            interrupt: false.into(),
            continue_context: Box::default(),
            pending_host_signals: AtomicU32::new(0),
            waiting_waker: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
            has_entered_guest: false.into(),
            fork_verify: RefCell::new(None),
            fork_verify_last_load: Cell::new(None),
            codewatch: fork_verify::CodewatchState::new(),
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
    // Clear EFLAGS.TF in the live CPU flags before anything else runs. The guest reaches here
    // via a call (the syscall rewriter's trampoline for every guest syscall instruction, not a
    // real syscall), which is itself the next instruction a fork() child under fork_verify
    // single-step verification was stepped through -- so if TF was armed, it is still live in
    // the CPU's real flags register at this point, and every subsequent host instruction here
    // (the register spills below, the call into the syscall handler, ...) would otherwise raise
    // its own single-step trap while is_in_guest is about to be (or has just been) cleared, i.e.
    // exactly the state vectored_exception_handler does not have a fork_verify handler for -- an
    // unhandled EXCEPTION_SINGLE_STEP (STATUS_SINGLE_STEP, 0x80000004) that tears down the whole
    // host process instead of just the child. pushfq/and/popfq on a scratch stack slot clears it
    // without disturbing any register (rax/rcx/r11 are all still live guest state here).
    pushfq
    and     QWORD PTR [rsp], 0xfffffffffffffeff
    popfq
    // Get the TLS state from the TLS slot and clear the in-guest flag.
    mov     r11d, DWORD PTR [rip + {TLS_INDEX}]
    mov     r11, QWORD PTR gs:[r11 * 8 + TEB_TLS_SLOTS_OFFSET]
    mov     BYTE PTR [r11 + {IS_IN_GUEST}], 0
    // Set rsp to the top of the guest context.
    mov     QWORD PTR [r11 + {SCRATCH}], rsp
    mov     rsp, QWORD PTR [r11 + {GUEST_CONTEXT_TOP}]

    // TODO: save float and vector registers (xsave or fxsave)
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
    SCRATCH = const core::mem::offset_of!(TlsState, scratch),
    IS_IN_GUEST = const core::mem::offset_of!(TlsState, is_in_guest),
    );
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

    // Restore fsbase for the guest.
    WindowsUserland::restore_thread_fs_base();

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
        tls.is_in_guest.set(true);
        switch_to_guest_sysret(ctx)
    } else {
        tls.has_entered_guest.set(true);
        switch_to_guest_ntcontinue(tls, ctx)
    }
}

fn thread_start(
    init_thread: Box<
        dyn litebox::shim::InitThread<ExecutionContext = litebox_common_linux::PtRegs>,
    >,
    mut ctx: litebox_common_linux::PtRegs,
) {
    // Allow caller to run some code before we return to the new thread.
    let shim = init_thread.init();

    run_thread_inner(shim.as_ref(), &mut ctx);
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
        let ctx = ctx.clone();
        // TODO: do we need to wait for the handle in the main thread?
        let _handle = std::thread::Builder::new().spawn(move || thread_start(init_thread, ctx))?;

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

    // Pick one arbitrary thread to deliver the signal to.
    let thread = ACTIVE_THREADS.lock().unwrap().first().cloned();

    if let Some(thread) = thread {
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
            let thread = ACTIVE_THREADS.lock().unwrap().first().cloned();
            if let Some(thread) = thread {
                thread.deliver_signal(litebox_common_linux::signal::Signal::SIGWINCH);
            }
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

/// Global registry of all active managed thread handles.
///
/// Threads are registered in [`ThreadHandle::run_with_handle`] and
/// removed when the guard drops.
///
/// TODO: This global list only works when we support a single process. For
/// multi-process support, each process (or `WindowsUserland` instance) should
/// track its own thread list.
static ACTIVE_THREADS: Mutex<alloc::vec::Vec<ThreadHandle>> = Mutex::new(alloc::vec::Vec::new());

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
        /// Helper to lock two mutexes in address order, to prevent deadlock.
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

        // Suspend the target thread.
        unsafe {
            windows_sys::Win32::System::Threading::SuspendThread(inner.handle.as_raw_handle());
        }
        let _resume_guard = litebox::utils::defer(|| unsafe {
            windows_sys::Win32::System::Threading::ResumeThread(inner.handle.as_raw_handle());
        });

        // SAFETY: The target TLS state is accessible while the thread is
        // suspended.
        let target_tls = unsafe { &*inner.tls.0 };

        // Write the target interrupt flag.
        target_tls.interrupt.set(true);

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

        let run_interrupt_callback = if (switch_to_guest_start as *const () as usize
            ..switch_to_guest_end as *const () as usize)
            .contains(&(context.Rip.trunc()))
        {
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

// A skeleton of a raw mutex for Windows.
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

    #[expect(clippy::unnecessary_wraps)]
    fn block_or_maybe_timeout(
        &self,
        val: u32,
        timeout: Option<Duration>,
    ) -> Result<UnblockedOrTimedOut, ImmediatelyWokenUp> {
        // Compute timeout in ms
        let timeout_ms = match timeout {
            None => Win32_Threading::INFINITE, // no timeout
            Some(timeout) => {
                let ms = timeout.as_millis();
                ms.min(u128::from(Win32_Threading::INFINITE - 1)).trunc()
            }
        };

        let ok = unsafe {
            Win32_Threading::WaitOnAddress(
                (&raw const self.inner).cast::<c_void>(),
                (&raw const val).cast::<c_void>(),
                std::mem::size_of::<u32>(),
                timeout_ms,
            ) != 0
        };

        if ok {
            Ok(UnblockedOrTimedOut::Unblocked)
        } else {
            // Check why WaitOnAddress failed
            let err = unsafe { GetLastError() };
            match err {
                Win32_Foundation::ERROR_TIMEOUT => Ok(UnblockedOrTimedOut::TimedOut),
                e => panic!("Unexpected error={e} for WaitOnAddress"),
            }
        }
    }
}

impl litebox::platform::RawMutex for RawMutex {
    const INIT: Self = Self::new();

    fn underlying_atomic(&self) -> &AtomicU32 {
        &self.inner
    }

    fn wake_many(&self, n: usize) -> usize {
        assert!(n > 0, "wake_many should be called with n > 0");
        let n: u32 = n.try_into().unwrap();

        let mutex = core::ptr::from_ref(self.underlying_atomic()).cast::<c_void>();
        unsafe {
            if n == 1 {
                Win32_Threading::WakeByAddressSingle(mutex);
            } else if n >= i32::MAX as u32 {
                Win32_Threading::WakeByAddressAll(mutex);
            } else {
                // Wake up `n` threads iteratively
                for _ in 0..n {
                    Win32_Threading::WakeByAddressSingle(mutex);
                }
            }
        }

        // For windows, the OS kernel does not tell us how many threads were actually woken up,
        // so we return zero to indicate that the count is unknown.
        0
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
                if litebox_common_linux::arch::is_valid_user_fs_base(val) {
                    // Use WindowsUserland's per-thread FS base management system
                    Self::set_thread_fs_base(val);
                    Ok(())
                } else {
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
        debug_assert_eq!(range.start, mbi.BaseAddress as usize);
        let len = mbi.RegionSize.min(range.len());
        let success = operation(range.start..range.start + len, mbi.State)?;
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

impl<const ALIGN: usize> litebox::platform::PageManagementProvider<ALIGN> for WindowsUserland {
    // TODO(chuqi): These are currently "magic numbers" grabbed from my Windows 11 SystemInformation.
    // The actual values should be determined by `GetSystemInfo()`.
    //
    // NOTE: make sure the values are PAGE_ALIGNED.
    const TASK_ADDR_MIN: usize = 0x1_0000;
    const TASK_ADDR_MAX: usize = 0x7FFF_FFFE_F000;
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

        // A helper closure to reserve and commit memory in one go.
        //
        // Note that MEM_RESERVE requires the base address to be aligned to system allocation granularity,
        // while MEM_COMMIT only requires page-aligned address.
        //
        // To ensure future MEM_COMMIT calls on sub-ranges succeed, we always reserve the entire aligned range
        // (i.e., MEM_RESERVE size is also made aligned to system allocation granularity).
        let reserve_and_commit = |r: core::ops::Range<usize>,
                                  flags: Win32_Memory::PAGE_PROTECTION_FLAGS|
         -> *mut c_void {
            let aligned_start_addr = self.round_down_to_granu(r.start);
            let aligned_end_addr = self.round_up_to_granu(r.end);
            let ptr = unsafe {
                VirtualAlloc2(
                    GetCurrentProcess(),
                    aligned_start_addr as *mut c_void,
                    aligned_end_addr - aligned_start_addr,
                    Win32_Memory::MEM_RESERVE,
                    Win32_Memory::PAGE_NOACCESS,
                    core::ptr::null_mut(),
                    0,
                )
            };
            if ptr.is_null() {
                core::ptr::null_mut()
            } else {
                unsafe {
                    VirtualAlloc2(
                        GetCurrentProcess(),
                        if r.start == 0 {
                            ptr
                        } else {
                            r.start as *mut c_void
                        },
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
        let size = suggested_range.len();
        // TODO: For Windows, there is no MAP_GROWDOWN features so far.
        let _ = can_grow_down;

        if suggested_range.start != 0 {
            assert!(suggested_range.start >= <WindowsUserland as litebox::platform::PageManagementProvider<ALIGN>>::
                                                            TASK_ADDR_MIN);
            assert!(suggested_range.end <= <WindowsUserland as litebox::platform::PageManagementProvider<ALIGN>>::
                                                            TASK_ADDR_MAX);

            let has_committed_page =
                process_memory_range_by_regions(suggested_range.clone(), |_r, state| {
                    if state == Win32_Memory::MEM_COMMIT {
                        Err(())
                    } else {
                        Ok(true)
                    }
                })
                .is_err();
            if has_committed_page && fixed_address_behavior == FixedAddressBehavior::Hint {
                // If any page in the suggested range is already committed, and the caller
                // did not request a fixed address, we ask the OS to allocate a new region.
                base_addr = core::ptr::null_mut();
            } else if has_committed_page
                && fixed_address_behavior == FixedAddressBehavior::NoReplace
            {
                return Err(AllocationError::AddressInUse);
            } else {
                process_memory_range_by_regions(
                    suggested_range,
                    |r, state| -> Result<bool, std::convert::Infallible> {
                        let ok = match state {
                            // In case the region is already reserved, we just need to commit it.
                            // In case the region is already committed, decommit and recommit it.
                            Win32_Memory::MEM_RESERVE | Win32_Memory::MEM_COMMIT => {
                                if state == Win32_Memory::MEM_COMMIT {
                                    // TODO: handle this race condition properly.
                                    assert_eq!(
                                        fixed_address_behavior,
                                        FixedAddressBehavior::Replace,
                                        "raced with another memory allocator"
                                    );
                                    let decommit_ok = unsafe {
                                        VirtualFree(
                                            r.start as *mut c_void,
                                            r.len(),
                                            Win32_Memory::MEM_DECOMMIT,
                                        )
                                    } != 0;
                                    assert!(
                                        decommit_ok,
                                        "VirtualFree(DECOMMIT) failed: {}",
                                        unsafe { GetLastError() }
                                    );
                                }
                                let ptr = unsafe {
                                    VirtualAlloc2(
                                        GetCurrentProcess(),
                                        r.start as *mut c_void,
                                        r.len(),
                                        Win32_Memory::MEM_COMMIT,
                                        prot_flags(initial_permissions),
                                        core::ptr::null_mut(),
                                        0,
                                    )
                                };
                                !ptr.is_null()
                            }
                            // In case the region is free, we need to reserve and commit it.
                            Win32_Memory::MEM_FREE => {
                                let ptr =
                                    reserve_and_commit(r.clone(), prot_flags(initial_permissions));
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
                return Ok(UserMutPtr::from_ptr(base_addr.cast()));
            }
        }

        debug_assert!(base_addr.is_null());
        let ptr = reserve_and_commit(0..size, prot_flags(initial_permissions));
        assert!(
            !ptr.is_null(),
            "VirtualAlloc2(RESERVE|COMMIT size=0x{:x}) failed: {}",
            size,
            std::io::Error::last_os_error()
        );

        // Prefetch the memory range if requested
        if populate_pages_immediately {
            do_prefetch_on_range(ptr as usize, size);
        }
        Ok(UserMutPtr::from_ptr(ptr.cast::<u8>()))
    }

    unsafe fn deallocate_pages(
        &self,
        range: core::ops::Range<usize>,
    ) -> Result<(), litebox::platform::page_mgmt::DeallocationError> {
        debug_assert_alignment!(range, ALIGN);
        process_memory_range_by_regions(
            range,
            |r, state| -> Result<bool, std::convert::Infallible> {
                debug_assert_ne!(
                    state,
                    Win32_Memory::MEM_FREE,
                    "Trying to deallocate a free region: {:p}-{:p}",
                    r.start as *mut c_void,
                    r.end as *mut c_void
                );
                Ok(unsafe {
                    VirtualFree(r.start as *mut c_void, r.len(), Win32_Memory::MEM_DECOMMIT)
                } != 0)
            },
        )
        .expect("deallocate_pages failed");
        Ok(())
    }

    unsafe fn update_permissions(
        &self,
        range: core::ops::Range<usize>,
        new_permissions: MemoryRegionPermissions,
    ) -> Result<(), litebox::platform::page_mgmt::PermissionUpdateError> {
        debug_assert_alignment!(range, ALIGN);
        let flags = prot_flags(new_permissions);
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
                Ok(unsafe {
                    VirtualProtect(r.start as *mut c_void, r.len(), flags, &raw mut old_protect)
                } != 0)
            },
        )
        .expect("update_permissions failed");
        Ok(())
    }

    fn reserved_pages(&self) -> impl Iterator<Item = &std::ops::Range<usize>> {
        self.reserved_pages.iter()
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
        let try_map = |base_addr: *const c_void| unsafe {
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
        };
        let base_addr = if suggested_range.start == 0 {
            core::ptr::null()
        } else {
            suggested_range.start as *const c_void
        };
        let mut view = try_map(base_addr);
        // `Hint` means the platform may pick a different address if the hint isn't available
        // (matching `allocate_pages`'s handling of the same case): retry with no address hint
        // rather than surfacing an address collision as an error.
        if view.Value.is_null()
            && !base_addr.is_null()
            && fixed_address_behavior == FixedAddressBehavior::Hint
        {
            view = try_map(core::ptr::null());
        }
        if view.Value.is_null() {
            let err = unsafe { GetLastError() };
            if fixed_address_behavior == FixedAddressBehavior::NoReplace
                && err == Win32_Foundation::ERROR_INVALID_ADDRESS
            {
                return Err(SharedMemoryError::AddressInUse);
            }
            return Err(SharedMemoryError::OutOfMemory);
        }
        Ok(UserMutPtr::from_ptr(view.Value.cast::<u8>()))
    }

    unsafe fn unmap_shared_memory(
        &self,
        range: core::ops::Range<usize>,
    ) -> Result<(), SharedMemoryError> {
        debug_assert_alignment!(range, ALIGN);
        let ok = unsafe {
            UnmapViewOfFileEx(
                Win32_Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: range.start as *mut c_void,
                },
                0,
            )
        } != 0;
        if !ok {
            return Err(SharedMemoryError::Unaligned);
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
            std::thread::Builder::new()
                .name("litebox-console-stdin-reader".to_owned())
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

#[global_allocator]
static SLAB_ALLOC: litebox::mm::allocator::SafeZoneAllocator<'static, 34, WindowsUserland> =
    litebox::mm::allocator::SafeZoneAllocator::new();

impl litebox::mm::allocator::MemoryProvider for WindowsUserland {
    fn alloc(layout: &std::alloc::Layout) -> Option<(usize, usize)> {
        let size = core::cmp::max(
            layout.size().next_power_of_two(),
            // Note `mmap` provides no guarantee of alignment, so we double the size to ensure we
            // can always find a required chunk within the returned memory region.
            core::cmp::max(layout.align(), 0x1000) << 1,
        );

        match unsafe {
            VirtualAlloc2(
                GetCurrentProcess(),
                core::ptr::null_mut(),
                size,
                Win32_Memory::MEM_COMMIT | Win32_Memory::MEM_RESERVE,
                Win32_Memory::PAGE_READWRITE,
                core::ptr::null_mut(),
                0,
            )
        } {
            addr if addr.is_null() => None,
            addr => Some((addr as usize, size)),
        }
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
    thread_ctx.call_shim(|shim, ctx, _interrupt| shim.init(ctx));
}

unsafe extern "C-unwind" fn syscall_handler(thread_ctx: &mut ThreadContext<'_>) {
    thread_ctx.call_shim(|shim, ctx, _interrupt| shim.syscall(ctx));
}

unsafe extern "C-unwind" fn exception_handler(
    thread_ctx: &mut ThreadContext<'_>,
    exception_record: &EXCEPTION_RECORD,
) {
    let (exception, error_code, cr2) = match exception_record.ExceptionCode {
        Win32_Foundation::EXCEPTION_ACCESS_VIOLATION => {
            let info = exception_record.ExceptionInformation;
            let read_write_flag = info[0];
            let faulting_address = info[1];
            if read_write_flag == 0 && faulting_address == !0 {
                // This is probably a #GP, not a #PF.
                (Exception::GENERAL_PROTECTION_FAULT, 0, 0)
            } else {
                let error_code = 4 | if read_write_flag == 0 { 0 } else { 1 << 1 }; // PF error code: bit 1 = write
                (Exception::PAGE_FAULT, error_code, faulting_address)
            }
        }
        Win32_Foundation::EXCEPTION_ILLEGAL_INSTRUCTION => (Exception::INVALID_OPCODE, 0, 0),
        Win32_Foundation::EXCEPTION_BREAKPOINT => (Exception::BREAKPOINT, 0, 0),
        Win32_Foundation::EXCEPTION_INT_DIVIDE_BY_ZERO => (Exception::DIVIDE_ERROR, 0, 0),
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
    thread_ctx.call_shim(|shim, ctx, interrupt| {
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
                if ctxwatch::enabled() && self.ctx.orig_rax == 0x3d {
                    ctxwatch::arm(self.ctx);
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
}

impl litebox::platform::SystemInfoProvider for WindowsUserland {
    fn get_syscall_entry_point(&self) -> usize {
        syscall_callback as *const () as usize
    }

    fn get_vdso_address(&self) -> Option<usize> {
        // Windows doesn't have VDSO equivalent, return None
        None
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
