// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Diagnostic, self-armed hardware write-watchpoint (`LITEBOX_CTXWATCH=1`) on the calling
//! thread's own `PtRegs.rip` field, used to root-cause an intermittent `EXCEPTION_ACCESS_VIOLATION
//! rip=0` crash (a jump to a null `ctx.rip`) that repeated tracing has shown happens strictly
//! between `switch_to_guest`'s entry fast-path check (`ctx.rcx == ctx.rip`, confirmed valid) and
//! `switch_to_guest_sysret`'s own final `mov rcx, [rcx+0x80]` read of that same field a few
//! instructions later -- i.e. something zeroes `ctx.rip` in memory during that narrow window,
//! and the leading (not yet confirmed) hypothesis is a stray cross-thread write.
//!
//! This uses real x86-64 debug registers (`Dr0`/`Dr7`), armed via `SetThreadContext` on the
//! CURRENT thread only -- no external debugger attaches, since prior passes found that both `cdb`
//! attachment and naive `eprintln!` tracing perturb this bug's tight timing window badly enough to
//! mask it (see `scratchpad/jqrepro/FINDINGS.txt`, PASS 17+/PASS 18). Setting one's own debug
//! registers is an ordinary instruction, not a suspend-and-inspect from another process, so it
//! should not carry the same timing hazard.
//!
//! A **write** watchpoint on `ctx.rip` should never legitimately fire: `switch_to_guest_sysret`
//! only *reads* that field, and no other code should be writing this specific stack-local `ctx`'s
//! `rip` field at all once the fast-path guard has already observed it non-zero. If the
//! watchpoint fires, that is itself the finding -- capturing which thread and which instruction
//! did the write pins the corruption's source directly.

use core::cell::Cell;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use windows_sys::Win32::System::Diagnostics::Debug::{
    CONTEXT, CONTEXT_DEBUG_REGISTERS_AMD64, GetThreadContext, SetThreadContext,
};
use windows_sys::Win32::System::Threading::GetCurrentThread;

/// Offset of `PtRegs::rip` within the struct, matching `switch_to_guest_sysret`'s own
/// `[rcx + 0x80]` field-offset comment.
pub(super) const RIP_FIELD_OFFSET: usize = 0x80;

/// Backing state for this watchpoint, held as a [`crate::TlsState`] field rather than a bare
/// `static`/`thread_local!` -- matches [`crate::fork_verify`]'s `codewatch` module's own reasoning
/// (see that module's doc comment): keeps this diagnostic off the crate's ratcheted bare-static
/// count, and per-thread is its natural scope anyway (each host OS thread has its own debug
/// registers, and only the thread arming the watchpoint ever needs to recognize/disarm its own).
pub(crate) struct State {
    /// The address currently armed on this thread (0 if none).
    armed_addr: Cell<usize>,
    /// Diagnostics-only counters, safe to read from any thread after a hit.
    hit_count: AtomicUsize,
    last_hit_writer_tid: AtomicU64,
}

impl State {
    pub(crate) const fn new() -> Self {
        Self {
            armed_addr: Cell::new(0),
            hit_count: AtomicUsize::new(0),
            last_hit_writer_tid: AtomicU64::new(0),
        }
    }
}

/// Runs `f` against the calling thread's watchpoint state, or returns `default` if this thread
/// has no `TlsState` yet (the watchpoint is only ever armed from a thread that does).
fn with<R>(default: R, f: impl FnOnce(&State) -> R) -> R {
    match crate::get_tls_ptr() {
        // SAFETY: `get_tls_ptr` returns this thread's live `TlsState`.
        Some(tls) => f(&unsafe { &*tls }.ctxwatch),
        None => default,
    }
}

pub(super) fn enabled() -> bool {
    std::env::var_os("LITEBOX_CTXWATCH").is_some()
}

/// Builds the `Dr0`/`Dr7`/`Dr6` fields for an 8-byte write watchpoint on `addr`, applied to an
/// otherwise-zeroed `CONTEXT_DEBUG_REGISTERS_AMD64` context. Shared by both the current-thread
/// arm path (`arm`) and the all-threads path (`arm_on_handle`, driven by `lib.rs` for every
/// OTHER live thread) so the exact same encoding is used everywhere -- debug registers are
/// per-thread on Windows (virtualized via `Get`/`SetThreadContext`), so a watchpoint armed only
/// on the calling (shell) thread can never observe a write made by an instruction executing on a
/// different OS thread, e.g. a pipeline child's own teardown code running on its own thread.
fn build_watch_context(addr: usize) -> CONTEXT {
    let mut context = CONTEXT {
        ContextFlags: CONTEXT_DEBUG_REGISTERS_AMD64,
        ..unsafe { core::mem::zeroed() }
    };
    context.Dr0 = addr as u64;
    // Dr7 encoding (x86-64 debug control register):
    //   bit 0  (L0)      = 1  -- local enable for breakpoint 0
    //   bits 16-17 (R/W0) = 01 -- break on data write only
    //   bits 18-19 (LEN0) = 10 -- 8-byte length
    // All other breakpoints (1-3) left disabled; LE/GE (bits 8-9) and the reserved bit 10 are
    // set for compatibility with older documentation/tooling, though modern CPUs ignore LE/GE.
    let rw0_write: u64 = 0b01;
    let len0_8bytes: u64 = 0b10;
    context.Dr7 = (1 << 0) | (rw0_write << 16) | (len0_8bytes << 18) | (1 << 10);
    context.Dr6 = 0;
    context
}

/// Arms a hardware write-watchpoint on the calling thread covering the 8-byte `rip` field of the
/// `PtRegs` at `ctx`. A `SetThreadContext` failure is reported but non-fatal -- this is
/// diagnostic-only.
pub(super) fn arm(ctx: *const litebox_common_linux::PtRegs) {
    if !enabled() {
        return;
    }
    let addr = (ctx as usize).wrapping_add(RIP_FIELD_OFFSET);

    // SAFETY: `GetCurrentThread` returns a pseudo-handle valid for the lifetime of the call;
    // `GetThreadContext`/`SetThreadContext` on it operate on the calling thread's own registers.
    unsafe {
        let mut context = CONTEXT {
            ContextFlags: CONTEXT_DEBUG_REGISTERS_AMD64,
            ..core::mem::zeroed()
        };
        if GetThreadContext(GetCurrentThread(), &raw mut context) == 0 {
            eprintln!(
                "[ctxwatch] tid={:?} GetThreadContext failed: {}",
                std::thread::current().id(),
                std::io::Error::last_os_error(),
            );
            return;
        }

        let watch = build_watch_context(addr);
        context.Dr0 = watch.Dr0;
        context.Dr7 = watch.Dr7;
        context.Dr6 = watch.Dr6;

        if SetThreadContext(GetCurrentThread(), &raw const context) == 0 {
            eprintln!(
                "[ctxwatch] tid={:?} SetThreadContext (arm) failed: {}",
                std::thread::current().id(),
                std::io::Error::last_os_error(),
            );
            return;
        }
    }
    with((), |s| s.armed_addr.set(addr));
    eprintln!(
        "[ctxwatch] tid={:?} armed write watch on {:#x}",
        std::thread::current().id(),
        addr,
    );
}

/// Arms the SAME watchpoint (`addr`, matching whatever `arm` used on the calling thread) on a
/// DIFFERENT, already-suspended OS thread via its raw handle. Callers are responsible for
/// suspending the target thread first and resuming it afterward (mirrors
/// `ThreadHandle::interrupt`'s existing `SuspendThread`/`Get`/`SetThreadContext`/`ResumeThread`
/// pattern in `lib.rs`, reused here rather than duplicated). Returns whether the arm succeeded;
/// failures are reported but non-fatal, same as `arm`.
///
/// # Safety
/// `handle` must be a valid, currently-suspended thread handle with `THREAD_SET_CONTEXT` /
/// `THREAD_GET_CONTEXT` access.
pub(super) unsafe fn arm_on_handle(
    handle: windows_sys::Win32::Foundation::HANDLE,
    addr: usize,
) -> bool {
    // SAFETY: caller guarantees `handle` is valid and the thread is suspended.
    unsafe {
        let mut context = CONTEXT {
            ContextFlags: CONTEXT_DEBUG_REGISTERS_AMD64,
            ..core::mem::zeroed()
        };
        if GetThreadContext(handle, &raw mut context) == 0 {
            eprintln!(
                "[ctxwatch] cross-thread GetThreadContext failed: {}",
                std::io::Error::last_os_error(),
            );
            return false;
        }
        let watch = build_watch_context(addr);
        context.Dr0 = watch.Dr0;
        context.Dr7 = watch.Dr7;
        context.Dr6 = watch.Dr6;
        if SetThreadContext(handle, &raw const context) == 0 {
            eprintln!(
                "[ctxwatch] cross-thread SetThreadContext (arm) failed: {}",
                std::io::Error::last_os_error(),
            );
            return false;
        }
    }
    eprintln!("[ctxwatch] cross-thread armed write watch on {addr:#x}");
    true
}

/// Disarms the calling thread's watchpoint, if any. Safe to call even if never armed.
pub(super) fn disarm() {
    let addr = with(0, |s| s.armed_addr.replace(0));
    if addr == 0 {
        return;
    }
    // SAFETY: same pseudo-handle/current-thread reasoning as `arm`.
    unsafe {
        let mut context = CONTEXT {
            ContextFlags: CONTEXT_DEBUG_REGISTERS_AMD64,
            ..core::mem::zeroed()
        };
        if GetThreadContext(GetCurrentThread(), &raw mut context) == 0 {
            return;
        }
        context.Dr7 &= !1u64; // clear L0
        context.Dr0 = 0;
        let _ = SetThreadContext(GetCurrentThread(), &raw const context);
    }
}

/// Whether `addr` is the address currently armed on the calling thread.
fn is_armed_here(addr: usize) -> bool {
    with(false, |s| s.armed_addr.get() == addr && addr != 0)
}

/// Handles an `EXCEPTION_SINGLE_STEP` that might be this watchpoint firing. `Dr6` bit 0 (`B0`)
/// indicates breakpoint 0 tripped. On real x86-64 hardware, a data write breakpoint traps *after*
/// the write completes, so the new value is already visible in memory -- no extra single-step is
/// needed to observe it. Logs the writer thread, its `Rip`, and the value now at the watched
/// address, then clears the trap (the watchpoint stays armed for one more potential hit; the
/// caller may choose to disarm afterward if only one hit is wanted).
///
/// Returns whether this trap belonged to the ctxwatch mechanism (and so has been handled).
pub(crate) fn on_possible_hit(context: &mut CONTEXT) -> bool {
    if !enabled() {
        return false;
    }
    // Windows' exception dispatch always populates `ContextRecord.Dr0`/`Dr6`/`Dr7` for a real
    // hardware-debug-register trap (`EXCEPTION_SINGLE_STEP` raised by Dr7, as opposed to an
    // `int3`/`EFLAGS.TF` software single-step) -- no extra `GetThreadContext` round-trip needed,
    // confirmed against a standalone minimal repro of this exact mechanism. Using `context`
    // directly here is also lower overhead and avoids a second syscall on what is meant to be a
    // rare, latency-sensitive diagnostic path.
    let (dr6, dr0) = (context.Dr6, context.Dr0);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "diagnostic-only; this platform is x86_64-only, addresses fit in usize"
    )]
    let addr = dr0 as usize;
    if dr6 & 1 == 0 || !is_armed_here(addr) {
        return false;
    }

    with((), |s| {
        s.hit_count.fetch_add(1, Ordering::Relaxed);
        // `ThreadId::as_u64` is unstable on this toolchain; hash the `Debug` representation
        // instead (diagnostic-only, does not need to be a "real" numeric thread id).
        let tid = {
            use core::hash::{Hash as _, Hasher as _};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::thread::current().id().hash(&mut hasher);
            hasher.finish()
        };
        s.last_hit_writer_tid.store(tid, Ordering::Relaxed);
    });

    // The write already completed (hardware data breakpoints trap post-write); read the new value
    // directly.
    let new_value = unsafe { core::ptr::read_unaligned(addr as *const u64) };

    eprintln!(
        "[ctxwatch] HIT writer_tid={:?} writer_rip={:#x} watched_addr={:#x} new_value={:#x}",
        std::thread::current().id(),
        context.Rip,
        addr,
        new_value,
    );

    // Clear Dr6 (sticky until explicitly cleared, per Intel SDM Vol 3B 17.2.4) directly on
    // `ContextRecord` -- this write-back takes effect when the exception dispatcher resumes the
    // thread via `NtContinue` with this (possibly modified) context, exactly like every other
    // `context.Xxx = ...` mutation this crate's VEH handler already relies on elsewhere (e.g.
    // `context.EFlags &= !eflags_tf` a few lines above the call site). No separate
    // `SetThreadContext` round-trip needed.
    context.Dr6 = 0;

    true
}
