// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Diagnostic, self-armed hardware write-watchpoint machinery used across many passes of
//! `scratchpad/jqrepro/FINDINGS.txt`'s investigation into an intermittent, guest-visible
//! `EXCEPTION_ACCESS_VIOLATION rip=0` crash (a jump to address 0).
//!
//! HISTORY (see `FINDINGS.txt` for the full account): passes 20-25 used [`arm`]/[`arm_on_handle`]
//! to watch the HOST-side `ctx.rip` field of the resuming thread's `PtRegs` -- the leading
//! hypothesis at the time was that some write zeroed that field between `switch_to_guest`'s entry
//! fast-path check and `switch_to_guest_sysret`'s final `[rcx+0x80]` read of it. That watchpoint
//! never fired across 100+ armed-and-crashed trials. Pass 28 then read the VEH-reported
//! `ExceptionInformation` directly at fault time and found `av_type == EXCEPTION_EXECUTE_FAULT`
//! at `av_addr == 0` with the saved host context otherwise intact: the GUEST itself branches to
//! null (most often via a `ret` that popped a zeroed word off its own stack), which retroactively
//! explains why the host-side `ctx.rip` watch never fired -- it was watching the wrong address
//! the whole time. Pass 30 added [`arm_addr`], a generic (not `PtRegs`-specific) arm entry point,
//! so the crash handler can reactively watch the actual implicated GUEST stack slot
//! (`faulting_rsp - 8`) computed fresh from each run's own first crash, rather than a fixed,
//! pre-guessed host address.
//!
//! This uses real x86-64 debug registers (`Dr0`/`Dr7`), armed via `SetThreadContext` on the
//! CURRENT thread only -- no external debugger attaches, since prior passes found that both `cdb`
//! attachment and naive `eprintln!` tracing perturb this bug's tight timing window badly enough to
//! mask it (see `scratchpad/jqrepro/FINDINGS.txt`, PASS 17+/PASS 18). Setting one's own debug
//! registers is an ordinary instruction, not a suspend-and-inspect from another process, so it
//! should not carry the same timing hazard.

use core::cell::Cell;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use windows_sys::Win32::System::Diagnostics::Debug::{
    CONTEXT, CONTEXT_DEBUG_REGISTERS_AMD64, GetThreadContext, M128A, SetThreadContext,
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
    /// Whether [`arm_fixed_on_current_thread`] has already armed `Dr1` on this thread. `Dr1` is
    /// never cleared by [`disarm`] (see that fixed-watch mechanism's own doc comment), so once
    /// set on a thread it stays armed for that thread's whole lifetime -- re-arming on every
    /// `call_shim` resume (as pass 48 originally did) is redundant after the first time and, per
    /// pass 53/54, expensive enough to perturb some repros' timing before they ever reach the
    /// code the watch exists to observe.
    fixed_armed: Cell<bool>,
}

impl State {
    pub(crate) const fn new() -> Self {
        Self {
            armed_addr: Cell::new(0),
            hit_count: AtomicUsize::new(0),
            last_hit_writer_tid: AtomicU64::new(0),
            fixed_armed: Cell::new(false),
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
    arm_addr(addr);
}

/// Arms a hardware write-watchpoint on the calling thread covering an arbitrary 8-byte-aligned
/// `addr`. Unlike [`arm`], not gated on `enabled()` (`LITEBOX_CTXWATCH`) -- pass-30's reactive
/// guest-stack-slot watch (armed from the `LITEBOX_DIAG_WAIT4GATE`-gated `[diag-rip0]` crash
/// handler, a separate diagnostic) uses this directly so the two diagnostics stay independently
/// controllable. A `SetThreadContext` failure is reported but non-fatal -- this is
/// diagnostic-only. Always overwrites whatever this thread's `armed_addr`/hit-detection state
/// currently holds, so at most one watch is tracked as "ours" per thread at a time.
pub(super) fn arm_addr(addr: usize) {
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

/// Temporary (see `FINDINGS.txt` PASS 48): fixed-address watch on debug register 1 (`Dr1`),
/// deliberately independent of the `Dr0`-based `arm_addr`/`disarm` pair above so the two
/// mechanisms cannot clobber each other -- `disarm()` (called unconditionally on every
/// guest-exit) only ever touches `Dr0`/`Dr7`'s L0 bit, never `Dr1`/L1, so a `Dr1` watch armed
/// here survives across the whole process lifetime once set, with no per-cycle re-arming
/// needed. Gated on `LITEBOX_DIAG_WATCHADDR=<hex address>` (no default; a bare "1" is invalid
/// input and left unarmed to avoid guessing a wrong bug-specific address into future builds).
/// Remove once the corrupting write behind PASS 48's investigation is caught or the address
/// hypothesis is retired.
fn diag_watchaddr_target() -> usize {
    static TARGET: AtomicUsize = AtomicUsize::new(usize::MAX);
    let cached = TARGET.load(Ordering::Relaxed);
    if cached != usize::MAX {
        return cached;
    }
    let parsed = std::env::var("LITEBOX_DIAG_WATCHADDR")
        .ok()
        .and_then(|s| usize::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0);
    TARGET.store(parsed, Ordering::Relaxed);
    parsed
}

/// Arms a `Dr1` 8-byte write watchpoint on `LITEBOX_DIAG_WATCHADDR` (if set) on the calling
/// thread. No-op if the env var is unset/unparseable. Idempotent-safe to call repeatedly (e.g.
/// once per live thread at thread-start) -- always re-applies the same encoding.
///
/// PASS 54 (see `FINDINGS.txt`): this used to be called unconditionally from `call_shim`'s
/// per-syscall `Resume` path, i.e. once per syscall return for the ENTIRE process lifetime of
/// every thread. `Dr1` is never cleared by `ctxwatch::disarm()` (see this module's own doc
/// comment above), so re-arming it on every single resume was pure waste beyond the very first
/// call on each thread -- and worse, pass 53 found that for the cheap `pty.spawn` repro
/// specifically, merely having this watch armed (via the resulting `GetThreadContext`/
/// `SetThreadContext` pair repeated hundreds of times during python3's own interpreter startup,
/// well before the guest ever calls `fork()`) was enough to change guest-visible timing so much
/// the repro never reached `fork()` at all within a 40s window. Skipping the redundant re-arms
/// removes that overhead while preserving the exact guarantee the original comment wanted: the
/// watch is still set before the first guest instruction ever runs on this thread, because the
/// one-shot arm below still happens on this thread's very first `Resume`.
pub(super) fn arm_fixed_on_current_thread() {
    let addr = diag_watchaddr_target();
    if addr == 0 {
        return;
    }
    if !with(false, |s| {
        if s.fixed_armed.get() {
            false
        } else {
            s.fixed_armed.set(true);
            true
        }
    }) {
        return;
    }
    unsafe {
        let mut context = CONTEXT {
            ContextFlags: CONTEXT_DEBUG_REGISTERS_AMD64,
            ..core::mem::zeroed()
        };
        if GetThreadContext(GetCurrentThread(), &raw mut context) == 0 {
            eprintln!(
                "[ctxwatch-fixed] tid={:?} GetThreadContext failed: {}",
                std::thread::current().id(),
                std::io::Error::last_os_error(),
            );
            return;
        }
        context.Dr1 = addr as u64;
        // Dr7: bit 2 (L1) local-enable for breakpoint 1; bits 20-21 (R/W1) = 01 write-only;
        // bits 22-23 (LEN1) = 10 (8 bytes). Preserve whatever L0/Dr0 config (the separate
        // `ctxwatch` Dr0 mechanism) is already present in `context.Dr7`/`context.Dr0` from the
        // `GetThreadContext` read above -- this only ORs in the L1 bits, never clears L0.
        let rw1_write: u64 = 0b01;
        let len1_8bytes: u64 = 0b10;
        context.Dr7 |= (1 << 2) | (rw1_write << 20) | (len1_8bytes << 22) | (1 << 10);
        if SetThreadContext(GetCurrentThread(), &raw const context) == 0 {
            eprintln!(
                "[ctxwatch-fixed] tid={:?} SetThreadContext (arm Dr1) failed: {}",
                std::thread::current().id(),
                std::io::Error::last_os_error(),
            );
            return;
        }
    }
    eprintln!(
        "[ctxwatch-fixed] tid={:?} armed Dr1 write watch on {:#x}",
        std::thread::current().id(),
        addr,
    );
}

/// Handles a possible `Dr1` (fixed-address) watch hit, reported via `Dr6` bit 1 (`B1`). Returns
/// whether this trap belonged to this mechanism. Deliberately does not disarm afterward (the
/// investigation wants every hit across the whole run, not just the first).
pub(crate) fn on_possible_fixed_hit(context: &mut CONTEXT) -> bool {
    let target = diag_watchaddr_target();
    if target == 0 || context.Dr6 & 0b10 == 0 {
        return false;
    }
    let new_value = unsafe { core::ptr::read_unaligned(target as *const u64) };
    // Also dump the 8 bytes immediately below the watched slot (the sibling `prev`/`self`-shaped
    // field for a struct-pthread-layout write, at offset -8) and above it (offset +8), plus
    // `rdi`/`rsi` (common argument registers for whatever function performed this write) -- cheap,
    // diagnostic-only, and helps distinguish "this write's whole 16/24-byte neighborhood is
    // internally consistent host-address garbage" from "only this one 8-byte slot is wrong".
    let below = unsafe { core::ptr::read_unaligned((target - 8) as *const u64) };
    let above = unsafe { core::ptr::read_unaligned((target + 8) as *const u64) };
    // Dump the actual code bytes at/before the reported `Rip` (post-write, so the faulting
    // `movups`/`mov` instruction is a few bytes BEFORE this) directly from live guest memory --
    // hand-disassembling a stripped musl .so via a static `objdump` risks misattributing
    // interleaved/no-symbol code (confirmed a problem this pass); reading the live bytes removes
    // that ambiguity entirely.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "diagnostic-only; this platform is x86_64-only, addresses fit in usize"
    )]
    let rip = context.Rip as usize;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "diagnostic-only; this platform is x86_64-only, addresses fit in usize"
    )]
    let rdi = context.Rdi as usize;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "diagnostic-only; this platform is x86_64-only, addresses fit in usize"
    )]
    let rsp = context.Rsp as usize;
    let mut code = [0u8; 96];
    let code_start = rip.wrapping_sub(48);
    for (i, b) in code.iter_mut().enumerate() {
        *b = unsafe { core::ptr::read_unaligned((code_start + i) as *const u8) };
    }
    // Dump a window of memory around `rdi`/`rsp` too -- if `rdi` already holds the same
    // host-range value as one of its own fields, that proves the corruption predates this
    // particular `movups` (it's just propagating an already-bad value), rather than this
    // instruction/its xmm0 source being the original point of corruption.
    let mut rdi_window = [0u64; 6];
    let rdi_base = rdi.wrapping_sub(16);
    for (i, w) in rdi_window.iter_mut().enumerate() {
        *w = unsafe { core::ptr::read_unaligned((rdi_base + i * 8) as *const u64) };
    }
    let mut rsp_window = [0u64; 64];
    for (i, w) in rsp_window.iter_mut().enumerate() {
        *w = unsafe { core::ptr::read_unaligned((rsp + i * 8) as *const u64) };
    }
    // PASS 49: dump every xmm register, not just xmm0 -- pass 48 established xmm0 already held
    // the poisoned value at the `movups` write but never located the load that put it there; if
    // the true load target a different xmm register that got moved into xmm0 by an instruction
    // outside the previously-captured 24-byte code window, this directly reveals it.
    let xmm_all: [M128A; 16] = unsafe { context.Anonymous.FltSave.XmmRegisters };
    let xmm0 = xmm_all[0];
    // REMOVED (PASS 59, see FINDINGS.txt): this used to unconditionally dump musl's
    // `__copy_tls`-adjacent globals at three bare, hardcoded absolute addresses (`0xe64890`,
    // `0xe64898`, `0xe648a0`) that pass 48's own comment already flagged as really needing to be
    // `alloc_base + <module-relative offset>` (`alloc_base` = ld-musl's own guest load base,
    // never actually computed or plumbed into this function) rather than a fixed literal --
    // i.e. these were always a hardcoded guess from one specific pass-48 run, not a real
    // computation. Pass 59 root-caused the years-long "arming a Dr1 watch causes the process to
    // stall forever" blocker (passes 53/54/57/58) directly to this: on a run where `alloc_base`
    // differs from pass 48's run, `0xe64890` lands on an unmapped host address, so the very first
    // Dr1 hit's diagnostic dump raises `EXCEPTION_ACCESS_VIOLATION`, which the VEH handler's
    // FS_BASE-reset repair (`lib.rs`, host-mode branch) retries in place -- but retrying re-runs
    // this exact same bad read, which faults identically forever, an unconditional infinite
    // repair loop with zero forward progress (confirmed via `LITEBOX_VEH_TRACE=1`: the trace
    // shows `mov 0xe64890,%rdx` as the faulting instruction, `is_in_guest=false`, looping without
    // ever reaching a second guest instruction). Removed rather than fixed-forward (e.g.
    // recomputing `alloc_base` here) because no caller of this function currently has that value
    // available, and this dump was already diagnostic-only extra context beyond the four
    // registers/xmm/code-window captures every hit already reports -- those are unaffected and
    // remain sufficient for the next live-watchpoint pass.
    let (rax, rbx, rcx, rdx, rsi, rbp, r12, r13) = (
        context.Rax,
        context.Rbx,
        context.Rcx,
        context.Rdx,
        context.Rsi,
        context.Rbp,
        context.R12,
        context.R13,
    );
    let (xmm0_low, xmm0_high) = (xmm0.Low, xmm0.High);
    eprintln!(
        "[ctxwatch-fixed] HIT writer_tid={:?} writer_rip={rip:#x} watched_addr={target:#x} \
         new_value={new_value:#x} below={below:#x} above={above:#x} rsp={rsp:#x} rax={rax:#x} \
         rbx={rbx:#x} rcx={rcx:#x} rdx={rdx:#x} rdi={rdi:#x} rsi={rsi:#x} rbp={rbp:#x} \
         r12={r12:#x} r13={r13:#x} code@rip-48={code:02x?} rdi_window@-16={rdi_window:x?} \
         rsp_window={rsp_window:x?} xmm0.Low={xmm0_low:#x} xmm0.High={xmm0_high:#x}",
        std::thread::current().id(),
    );
    for (i, x) in xmm_all.iter().enumerate() {
        eprintln!(
            "[ctxwatch-fixed] xmm{i}.Low={:#x} xmm{i}.High={:#x}",
            x.Low, x.High
        );
    }
    context.Dr6 &= !0b10;
    true
}

/// The address currently armed on the calling thread (0 if none). Used by a one-shot,
/// crash-time diagnostic in `vectored_exception_handler` to distinguish "the watched memory
/// really does contain 0" (a genuine overwrite -- the watchpoint should then also have fired,
/// which prior passes established it never does) from "the watched memory still holds the
/// correct, non-zero value" (proof that whatever jumped to `rip=0` read a DIFFERENT address
/// than the one that was armed/validated -- an aliasing/wrong-pointer bug, not a corruption).
pub(super) fn current_armed_addr() -> usize {
    with(0, |s| s.armed_addr.get())
}

/// Handles an `EXCEPTION_SINGLE_STEP` that might be this watchpoint firing. `Dr6` bit 0 (`B0`)
/// indicates breakpoint 0 tripped. On real x86-64 hardware, a data write breakpoint traps *after*
/// the write completes, so the new value is already visible in memory -- no extra single-step is
/// needed to observe it. Logs the writer thread, its `Rip`, and the value now at the watched
/// address, then clears the trap (the watchpoint stays armed for one more potential hit; the
/// caller may choose to disarm afterward if only one hit is wanted).
///
/// Returns whether this trap belonged to the ctxwatch mechanism (and so has been handled).
///
/// Not gated on `enabled()` (`LITEBOX_CTXWATCH`): pass-30's reactive guest-stack-slot watch,
/// armed via `arm_addr` from the separate `LITEBOX_DIAG_WAIT4GATE` diagnostic, must still be
/// able to report its own hits when `LITEBOX_CTXWATCH` is unset. `is_armed_here` below already
/// returns `false` (a no-op) whenever nothing is actually armed on this thread, so this stays
/// free when neither diagnostic is active.
pub(crate) fn on_possible_hit(context: &mut CONTEXT) -> bool {
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
