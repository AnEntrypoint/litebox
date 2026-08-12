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

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use windows_sys::Win32::System::Diagnostics::Debug::{
    CONTEXT, CONTEXT_DEBUG_REGISTERS_AMD64, GetThreadContext, SetThreadContext,
};
use windows_sys::Win32::System::Threading::GetCurrentThread;

/// Offset of `PtRegs::rip` within the struct, matching `switch_to_guest_sysret`'s own
/// `[rcx + 0x80]` field-offset comment.
const RIP_FIELD_OFFSET: usize = 0x80;

pub(super) fn enabled() -> bool {
    std::env::var_os("LITEBOX_CTXWATCH").is_some()
}

thread_local! {
    // The address currently armed on *this* thread (0 if none), so `disarm` and the VEH handler
    // can recognize a hit belongs to this mechanism without re-deriving the address. Per-thread
    // by construction (each host OS thread has its own debug registers), so a simple
    // thread-local is correct and avoids cross-thread false positives on the "was this armed"
    // check itself.
    static ARMED_ADDR: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

/// Diagnostics-only counters, safe to read from any thread after a hit.
static HIT_COUNT: AtomicUsize = AtomicUsize::new(0);
static LAST_HIT_WRITER_TID: AtomicU64 = AtomicU64::new(0);

/// Arms a hardware write-watchpoint on the calling thread covering the 8-byte `rip` field of the
/// `PtRegs` at `ctx`. Returns whether the arm succeeded (a `SetThreadContext` failure is reported
/// but non-fatal -- this is diagnostic-only).
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

        if SetThreadContext(GetCurrentThread(), &raw const context) == 0 {
            eprintln!(
                "[ctxwatch] tid={:?} SetThreadContext (arm) failed: {}",
                std::thread::current().id(),
                std::io::Error::last_os_error(),
            );
            return;
        }
    }
    ARMED_ADDR.with(|a| a.set(addr));
    eprintln!(
        "[ctxwatch] tid={:?} armed write watch on {:#x}",
        std::thread::current().id(),
        addr,
    );
}

/// Disarms the calling thread's watchpoint, if any. Safe to call even if never armed.
pub(super) fn disarm() {
    let addr = ARMED_ADDR.with(|a| a.replace(0));
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
    ARMED_ADDR.with(|a| a.get() == addr && addr != 0)
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

    HIT_COUNT.fetch_add(1, Ordering::Relaxed);
    // `ThreadId::as_u64` is unstable on this toolchain; hash the `Debug` representation instead
    // (diagnostic-only, does not need to be a "real" numeric thread id).
    let tid = {
        use core::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::thread::current().id().hash(&mut hasher);
        hasher.finish()
    };
    LAST_HIT_WRITER_TID.store(tid, Ordering::Relaxed);

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

/// Silences "unused" warnings for the diagnostics-only counters when `LITEBOX_CTXWATCH` tracing
/// never fires in a given run; kept as real state (not `#[allow(dead_code)]`) since a future pass
/// may want to read them back, e.g. from a panic handler.
#[allow(dead_code, reason = "diagnostic accessor, not yet consumed elsewhere")]
pub(super) fn last_hit_writer_tid() -> u64 {
    LAST_HIT_WRITER_TID.load(Ordering::Relaxed)
}

#[allow(dead_code, reason = "diagnostic accessor, not yet consumed elsewhere")]
pub(super) fn hit_count() -> usize {
    HIT_COUNT.load(Ordering::Relaxed)
}
