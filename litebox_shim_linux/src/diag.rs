// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Diagnostic instrumentation for the shim: an optional per-syscall summary
//! (`LITEBOX_STRACE_SUMMARY=1`) and an always-on guest process timeline/tree.
//!
//! This module is deliberately free of any `Platform`/`FS` generic parameter so its global
//! state can live in plain `static`s (the `no_std` idiom already used elsewhere in this
//! workspace, e.g. `litebox::sync::lock_tracing::EVENT_RECORDER`), gated at each call site by
//! [`litebox::platform::SystemInfoProvider::env_flag`] rather than a direct host env-var read
//! (this crate is `#![no_std]` and has no other way to observe the host process environment).

use alloc::collections::btree_map::BTreeMap;
use alloc::string::{String, ToString as _};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Per-syscall accounting, keyed by raw syscall number.
#[derive(Default)]
struct SyscallStats {
    count: u64,
    total_ns: u64,
    max_ns: u64,
    /// Histogram of `Debug`-formatted errno values seen on `Err` results.
    errors: BTreeMap<String, u64>,
}

/// A single "returned an unsupported-style errno" observation, recorded once per distinct
/// `(syscall/subcommand, errno)` pair with the first caller that hit it.
struct UnsupportedHit {
    first_pid: i32,
    first_comm: String,
    count: u64,
}

#[derive(Default)]
struct StraceSummary {
    by_syscall: BTreeMap<usize, SyscallStats>,
    /// Syscall numbers that failed to resolve via `SyscallRequest::try_from_raw` at all,
    /// keyed by raw number, recording the first caller.
    unresolved: BTreeMap<usize, UnsupportedHit>,
    /// `ENOSYS`/`EINVAL`/`ENOTSUP`/`EOPNOTSUPP`/`EPERM` sub-command misses, keyed by a
    /// free-form description (e.g. `"ioctl(TIOCGWINSZ)"`, `"prctl(PR_SET_NAME)"`).
    unsupported_subcommands: BTreeMap<String, UnsupportedHit>,
}

static STRACE_ENABLED: AtomicBool = AtomicBool::new(false);
static STRACE_INIT: AtomicBool = AtomicBool::new(false);
static STRACE_SUMMARY: spin::Mutex<StraceSummary> = spin::Mutex::new(StraceSummary {
    by_syscall: BTreeMap::new(),
    unresolved: BTreeMap::new(),
    unsupported_subcommands: BTreeMap::new(),
});

/// Errno names treated as "this syscall/sub-command isn't really supported" for the
/// dedicated unsupported-commands report section.
const NOTABLE_ERRNOS: &[&str] = &["ENOSYS", "EINVAL", "ENOTSUP", "EOPNOTSUPP", "EPERM"];

/// Call once, early (e.g. from the first syscall dispatch), with a closure performing the
/// platform's `env_flag` lookup for `LITEBOX_STRACE_SUMMARY`. Idempotent, and after the first
/// call costs one acquire load -- the closure is never run again.
pub fn init_strace_summary(enabled: impl FnOnce() -> bool) {
    // The parameter is a closure, not a `bool`, so the caller's `env_flag` lookup is not
    // evaluated on every call. It used to be: an eagerly-evaluated argument made this
    // "idempotent and cheap after the first call" latch cost a full host environment-variable
    // read per syscall per thread, which on Windows means a process-wide critical section and
    // two allocations -- the exact opposite of what the call site's own comment promised. The
    // `swap` is gone for the same reason: an unconditional read-modify-write on a shared cache
    // line, once per syscall on every guest thread, is not free either.
    if STRACE_INIT.load(Ordering::Acquire) {
        return;
    }
    // Racing callers are harmless: they read the same host environment and latch the same value.
    // `ENABLED` is published before `INIT` so a reader that observes the latch also observes the
    // value that was latched.
    STRACE_ENABLED.store(enabled(), Ordering::Release);
    STRACE_INIT.store(true, Ordering::Release);
}

pub fn strace_summary_enabled() -> bool {
    STRACE_ENABLED.load(Ordering::Acquire)
}

/// Unconditionally sets the live enabled state, unlike [`init_strace_summary`] -- that function
/// is a one-shot latch (a second call is a no-op by design, see its own doc comment), so it
/// cannot serve a genuine runtime `strace on`/`strace off` control-channel command
/// (`docs/presenter-process-design.md` section 3.2). This is that missing setter: cheap (one
/// store), safe to call from any thread at any time, and idempotent in the sense that matters for
/// a toggle (setting the same value twice is harmless), not in `init_strace_summary`'s
/// call-once-only sense.
pub fn set_strace_summary_enabled(enabled: bool) {
    STRACE_ENABLED.store(enabled, Ordering::Release);
    STRACE_INIT.store(true, Ordering::Release);
}

/// Record one completed syscall dispatch. `duration_ns` is the wall time spent in
/// `do_syscall`. `err_debug` is `Some(format!("{err:?}"))` on an `Err` result.
pub fn record_syscall(syscall_number: usize, duration_ns: u64, err_debug: Option<String>) {
    // These grow a `static` collection; keep its nodes in private memory so a native-`fork()`
    // child's copy of the static never aliases the parent's (see `PrivateAllocGuard`).
    let _private = litebox_util_log::PrivateAllocGuard::new();
    if !strace_summary_enabled() {
        return;
    }
    let mut guard = STRACE_SUMMARY.lock();
    let stats = guard.by_syscall.entry(syscall_number).or_default();
    stats.count += 1;
    stats.total_ns += duration_ns;
    if duration_ns > stats.max_ns {
        stats.max_ns = duration_ns;
    }
    if let Some(errno) = err_debug {
        *stats.errors.entry(errno).or_insert(0) += 1;
    }
}

/// Sampled `(syscall number, guest rip)` histogram: WHICH CALL SITE makes each syscall.
///
/// Built for "the browser main thread burns 100% CPU on ~16.5k syscalls/s, 85% of them
/// `clock_gettime` at one fixed `rip`". `LITEBOX_STRACE_SUMMARY`'s per-syscall table names the
/// syscall but not its caller, and a count alone cannot tell "one site looping" from "a million
/// sites each called once". Sampling every [`RIP_SAMPLE_EVERY`]-th dispatch keeps the per-syscall
/// cost at one relaxed atomic increment even at 16.5k/s, and a loop collapses to a single row with
/// an enormous count -- which is exactly the shape being looked for.
const RIP_SAMPLE_EVERY: u64 = 64;

/// How many SAMPLES (not dispatches) accumulate between periodic dumps: 2048 samples x 64 = one
/// dump per ~131k syscalls, i.e. roughly every 8 s on a thread doing 16.5k/s. Periodic because
/// the exit-time dump only fires when the bootstrap process *exits*, and the run being diagnosed
/// is killed by the harness's `timeout -s KILL` precisely because it never finishes.
const RIP_DUMP_EVERY_SAMPLES: u64 = 512;

struct RipHit {
    count: u64,
    /// Return address the trampoline pushed: the guest instruction after the original `syscall`.
    caller: u64,
    pid: i32,
    tid: i32,
    comm: String,
}

static RIP_DISPATCH_COUNT: AtomicU64 = AtomicU64::new(0);
static RIP_SAMPLE_COUNT: AtomicU64 = AtomicU64::new(0);
static RIP_DUMP_DUE: AtomicBool = AtomicBool::new(false);
static RIP_HISTOGRAM: spin::Mutex<BTreeMap<(usize, u64), RipHit>> =
    spin::Mutex::new(BTreeMap::new());
static RIP_ENABLED: AtomicBool = AtomicBool::new(false);
static RIP_INIT: AtomicBool = AtomicBool::new(false);

/// One-shot latch for `LITEBOX_RIP_HIST` (the periodic dump), same shape as
/// [`init_strace_summary`].
pub fn init_rip_hist(enabled: impl FnOnce() -> bool) {
    if RIP_INIT.load(Ordering::Acquire) {
        return;
    }
    RIP_ENABLED.store(enabled(), Ordering::Release);
    RIP_INIT.store(true, Ordering::Release);
}

fn rip_hist_enabled() -> bool {
    RIP_ENABLED.load(Ordering::Acquire)
}

/// Record the call site of one syscall dispatch. `rip` is the guest instruction pointer the
/// syscall was made from, which for a rewritten `syscall` is the trampoline, NOT the guest
/// function that made the call -- so `caller` is the return address that same trampoline pushed
/// (`[rsp]`), i.e. the instruction after the original `syscall`. Both are recorded because the
/// trampoline alone identifies the call site to litebox but not to anyone holding the binary.
pub fn record_syscall_rip(
    syscall_number: usize,
    rip: u64,
    caller: u64,
    pid: i32,
    tid: i32,
    comm: &str,
) {
    if !rip_hist_enabled() && !strace_summary_enabled() {
        return;
    }
    if !RIP_DISPATCH_COUNT
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(RIP_SAMPLE_EVERY)
    {
        return;
    }
    // These grow a `static` collection; keep its nodes in private memory so a native-`fork()`
    // child's copy of the static never aliases the parent's (see `PrivateAllocGuard`).
    let _private = litebox_util_log::PrivateAllocGuard::new();
    let mut guard = RIP_HISTOGRAM.lock();
    let hit = guard.entry((syscall_number, rip)).or_insert(RipHit {
        count: 0,
        caller,
        pid,
        tid,
        comm: comm.to_string(),
    });
    hit.count += 1;
    // `fetch_add` returns the PREVIOUS value, so 0 would trip the modulo on the very first
    // sample and dump a one-row table before anything has happened. Add 1 first.
    if (RIP_SAMPLE_COUNT.fetch_add(1, Ordering::Relaxed) + 1).is_multiple_of(RIP_DUMP_EVERY_SAMPLES)
    {
        RIP_DUMP_DUE.store(true, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// "Where is every guest thread parked?" (`LITEBOX_PARKED=1`)
//
// Built for the Linux-host chromium blocker where the whole session goes quiet: every host
// thread sits in `futex_do_wait`, `LITEBOX_STRACE_SUMMARY` only prints at bootstrap exit (which
// a harness `timeout -s KILL` prevents), and the rip histogram only ever sees syscalls that
// RETURN -- so a thread that entered a syscall and never came back is invisible to every
// existing instrument. This keeps a `(pid, tid) -> in-flight syscall` map, and dumps the rows
// that have been in flight longer than [`PARKED_MIN_MS`] on the first dispatch that happens at
// least [`PARKED_DUMP_EVERY_MS`] after the previous dump. It is driven by dispatches because
// this crate has no timer thread, and a fully quiet guest still produces one occasionally.
// ---------------------------------------------------------------------------

/// Below this, a syscall is just slow, not parked.
const PARKED_MIN_MS: u64 = 2000;
const PARKED_DUMP_EVERY_MS: u64 = 5000;

struct Inflight {
    host_pid: i32,
    guest_pid: i32,
    sysno: usize,
    args: [u64; 4],
    started_ms: u64,
    comm: [u8; 16],
}

/// Keyed by the ADDRESS OF THE `Task`, never by a guest pid/tid: `reinit_as_native_fork_child`
/// rewrites `self.tid` in the middle of the `clone` syscall, so a tid-keyed entry inserted on
/// entry can never be removed on exit -- it would sit there forever and read as "a thread parked
/// in `clone` for the whole run", which is exactly the false conclusion this instrument produced
/// once already. The `Task` address is the one identity a thread keeps across that rewrite.
static INFLIGHT: spin::Mutex<BTreeMap<u64, Inflight>> = spin::Mutex::new(BTreeMap::new());
static INFLIGHT_ENABLED: AtomicBool = AtomicBool::new(false);
static INFLIGHT_INIT: AtomicBool = AtomicBool::new(false);
static INFLIGHT_LAST_DUMP_MS: AtomicU64 = AtomicU64::new(0);

pub fn init_parked(enabled: impl FnOnce() -> bool) {
    if INFLIGHT_INIT.load(Ordering::Acquire) {
        return;
    }
    INFLIGHT_ENABLED.store(enabled(), Ordering::Release);
    INFLIGHT_INIT.store(true, Ordering::Release);
}

pub fn parked_enabled() -> bool {
    INFLIGHT_ENABLED.load(Ordering::Acquire)
}

/// Whether the "what is the MAIN thread doing" instrument is on (`LITEBOX_DIAG_MAINTHREAD=1`).
/// The main thread is the one whose tid equals its pid; a thread that leaves its message pump
/// and spins is invisible to every exit-time instrument, because it never parks and never
/// finishes the syscall the instruments would attribute to it.
static MAINTHREAD_ENABLED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
pub fn set_mainthread_enabled(v: bool) {
    MAINTHREAD_ENABLED.store(v, core::sync::atomic::Ordering::Release);
}
pub fn mainthread_enabled() -> bool {
    MAINTHREAD_ENABLED.load(core::sync::atomic::Ordering::Acquire)
}

/// The first version packed `(ms << 32) | pid` into ONE u64, and `ms << 32` OVERFLOWS for a
/// millisecond clock (ms since the epoch is ~1.8e12, so `<< 32` needs 73 bits): the stored
/// "previous ms" was silently truncated to its low 32 bits, `now - prev` was therefore always
/// enormous, and every dispatch emitted -- 38k lines in 45 s where the throttle allows 180.
/// Keep the two halves in separate atomics so no packing is needed.
const MAINTHREAD_SLOTS: usize = 64;
static MAINTHREAD_PID: [core::sync::atomic::AtomicU32; MAINTHREAD_SLOTS] =
    [const { core::sync::atomic::AtomicU32::new(0) }; MAINTHREAD_SLOTS];
static MAINTHREAD_MS: [core::sync::atomic::AtomicU64; MAINTHREAD_SLOTS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; MAINTHREAD_SLOTS];
const MAINTHREAD_THROTTLE_MS: u64 = 250;

/// Rate-limit one main-thread line for `pid` to [`MAINTHREAD_THROTTLE_MS`], per pid.
#[allow(
    clippy::cast_sign_loss,
    reason = "pid is a positive host pid; the u32 is only a slot key"
)]
pub fn mainthread_should_emit(pid: i32, now_ms: u64) -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    let key = pid as u32;
    for i in 0..MAINTHREAD_SLOTS {
        if MAINTHREAD_PID[i].load(Relaxed) != key {
            continue;
        }
        let prev = MAINTHREAD_MS[i].load(Relaxed);
        return now_ms.saturating_sub(prev) >= MAINTHREAD_THROTTLE_MS
            && MAINTHREAD_MS[i]
                .compare_exchange(prev, now_ms, Relaxed, Relaxed)
                .is_ok();
    }
    // Unseen pid: claim the first free slot (pid 0 is never a real pid, so it marks "free") and
    // emit. The first sighting of a main thread is worth one line even at t=0.
    for i in 0..MAINTHREAD_SLOTS {
        if MAINTHREAD_PID[i]
            .compare_exchange(0, key, Relaxed, Relaxed)
            .is_ok()
        {
            MAINTHREAD_MS[i].store(now_ms, Relaxed);
            return true;
        }
    }
    false
}

/// The same throttle for the stack dump, slower because one dump is 64 qwords.
static MAINTHREAD_STACK_MS: [core::sync::atomic::AtomicU64; MAINTHREAD_SLOTS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; MAINTHREAD_SLOTS];
static MAINTHREAD_STACK_PID: [core::sync::atomic::AtomicU32; MAINTHREAD_SLOTS] =
    [const { core::sync::atomic::AtomicU32::new(0) }; MAINTHREAD_SLOTS];
const MAINTHREAD_STACK_EVERY_MS: u64 = 2000;

#[allow(
    clippy::cast_sign_loss,
    reason = "pid is a positive host pid; the u32 is only a slot key"
)]
pub fn mainthread_should_dump_stack(pid: i32, now_ms: u64) -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    let key = pid as u32;
    for i in 0..MAINTHREAD_SLOTS {
        if MAINTHREAD_STACK_PID[i].load(Relaxed) != key {
            continue;
        }
        let prev = MAINTHREAD_STACK_MS[i].load(Relaxed);
        return now_ms.saturating_sub(prev) >= MAINTHREAD_STACK_EVERY_MS
            && MAINTHREAD_STACK_MS[i]
                .compare_exchange(prev, now_ms, Relaxed, Relaxed)
                .is_ok();
    }
    for i in 0..MAINTHREAD_SLOTS {
        if MAINTHREAD_STACK_PID[i]
            .compare_exchange(0, key, Relaxed, Relaxed)
            .is_ok()
        {
            MAINTHREAD_STACK_MS[i].store(now_ms, Relaxed);
            return true;
        }
    }
    false
}

/// One-shot latch for the guest VMA table dump. Printed once per run because every address the
/// stack dump yields is meaningless without the mapping it lands in: a qword in a 327 MB range
/// is the chromium binary, one in a ~2 MB range is libc, and neither can be turned into a file
/// offset without the mapping's own bias.
static VMA_DUMPED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
pub fn claim_vma_dump() -> bool {
    !VMA_DUMPED.swap(true, core::sync::atomic::Ordering::AcqRel)
}

/// Monotonic-ish wall clock in milliseconds, from the platform's SYSTEM clock (not its monotonic
/// `Instant`) so it needs no per-platform epoch stored in a `static` -- a generic `Instant`
/// cannot live in one, and this module has no `Platform` type parameter.
pub fn now_ms<Platform: litebox::platform::TimeProvider>(platform: &Platform) -> u64 {
    let t = platform.current_time();
    let d = match litebox::platform::SystemTime::duration_since(
        &t,
        &<Platform::SystemTime as litebox::platform::SystemTime>::UNIX_EPOCH,
    ) {
        Ok(d) | Err(d) => d,
    };
    d.as_millis().try_into().unwrap_or(u64::MAX)
}

/// A coarse "what step is this thread on" tag, set by the syscall implementations themselves and
/// printed alongside their row. `clone` needs it: a thread parked in `clone` could be waiting for
/// an admission slot, for one of the shim-wide locks `fork()` is taken under, or inside `fork()`
/// itself, and those have completely different fixes.
static THREAD_NOTES: spin::Mutex<BTreeMap<u64, &'static str>> = spin::Mutex::new(BTreeMap::new());

pub fn set_thread_note(task: u64, note: &'static str) {
    if !parked_enabled() {
        return;
    }
    let _private = litebox_util_log::PrivateAllocGuard::new();
    THREAD_NOTES.lock().insert(task, note);
}

pub fn clear_thread_note(task: u64) {
    if !parked_enabled() {
        return;
    }
    THREAD_NOTES.lock().remove(&task);
}

/// `&'static str` is `Copy`, so the value (not a borrow of the map) is what comes out and no
/// guard outlives this call.
pub fn thread_note(task: u64) -> Option<&'static str> {
    if !parked_enabled() {
        return None;
    }
    THREAD_NOTES.lock().get(&task).copied()
}

#[allow(
    clippy::too_many_arguments,
    reason = "one syscall entry's fields; a wrapper struct would only repack them"
)]
pub fn inflight_enter(
    task: u64,
    host_pid: i32,
    sysno: usize,
    args: [u64; 4],
    pid: i32,
    _tid: i32,
    comm: &[u8],
    now_ms: u64,
) {
    if !parked_enabled() {
        return;
    }
    let _private = litebox_util_log::PrivateAllocGuard::new();
    let mut c = [0u8; 16];
    let n = comm.len().min(16);
    c[..n].copy_from_slice(&comm[..n]);
    INFLIGHT.lock().insert(
        task,
        Inflight {
            host_pid,
            guest_pid: pid,
            sysno,
            args,
            started_ms: now_ms,
            comm: c,
        },
    );
}

pub fn inflight_exit(task: u64) {
    if !parked_enabled() {
        return;
    }
    INFLIGHT.lock().remove(&task);
}

pub fn maybe_dump_inflight<
    Platform: litebox::platform::TimeProvider + litebox::platform::StdioProvider,
>(
    platform: &Platform,
    now_ms: u64,
) {
    if !parked_enabled() {
        return;
    }
    let last = INFLIGHT_LAST_DUMP_MS.load(Ordering::Acquire);
    if now_ms.saturating_sub(last) < PARKED_DUMP_EVERY_MS {
        return;
    }
    INFLIGHT_LAST_DUMP_MS.store(now_ms, Ordering::Release);
    let guard = INFLIGHT.lock();
    let mut rows: Vec<(u64, &Inflight)> = guard
        .iter()
        .filter(|(_, f)| now_ms.saturating_sub(f.started_ms) >= PARKED_MIN_MS)
        .map(|(k, f)| (*k, f))
        .collect();
    rows.sort_by_key(|a| a.1.started_ms);
    emit_timeline_line(
        platform,
        &alloc::format!(
            "[diag-parked] {} thread(s) parked >= {PARKED_MIN_MS} ms (of {} in flight)",
            rows.len(),
            guard.len()
        ),
    );
    for (task, f) in rows.iter().take(40) {
        let end = f.comm.iter().position(|&b| b == 0).unwrap_or(16);
        emit_timeline_line(
            platform,
            &alloc::format!(
                "[diag-parked] parked_ms={} host_pid={} pid={} tid={} note={} syscall={}({}) a0={:#x} a1={:#x} a2={:#x} a3={:#x}",
                now_ms.saturating_sub(f.started_ms),
                f.host_pid,
                f.guest_pid,
                alloc::string::String::from_utf8_lossy(&f.comm[..end]),
                thread_note(*task).unwrap_or("-"),
                syscall_name(f.sysno),
                f.sysno,
                f.args[0],
                f.args[1],
                f.args[2],
                f.args[3],
            ),
        );
    }
}

// ---------------------------------------------------------------------------
// "Which syscall is each guest thread parked in?" (`LITEBOX_DIAG_STUCK_SECS=<secs>`)
//
// `LITEBOX_PARKED`'s `INFLIGHT` map is a process-private `static`, and its dump hangs off the
// DISPATCH-EXIT hook -- so a native-`fork()` child that enters one syscall and never returns
// (a chromium renderer) can neither be seen by another process nor ever reach the hook itself.
// This keeps the same record in the cross-process SHARED kernel arena, allocated once from
// `LinuxShimBuilder::build`'s create branch exactly as `SharedRecordLockTable` is, so the mapping
// is inherited by every `fork()` descendant at the same address and ANY process still dispatching
// prints the whole family's rows. Every field is a plain atomic: the dump takes no lock, so it
// still runs while some other thread sits on one.
// ---------------------------------------------------------------------------

const STUCK_SLOTS: usize = 512;
/// Linear-probe depth when claiming or finding a slot.
const STUCK_PROBE: usize = 8;
/// Used when `LITEBOX_DIAG_STUCK_SECS` is set to something unparseable.
const STUCK_DEFAULT_SECS: u64 = 10;
/// Distinct syscall numbers a slot's mix histogram keeps.
const STUCK_MIX: usize = 8;
/// `mix_sysno` marker for an unused histogram entry (a real syscall number is never `u32::MAX`).
const STUCK_MIX_FREE: u32 = u32::MAX;

#[repr(C)]
struct StuckSlot {
    /// Slot key: a guest `(pid, tid)` pair is unique across the whole fork family, unlike a `Task`
    /// address (a `Task` lives in process-private memory, so two processes' tasks collide) or a
    /// host pid (`current_host_pid` is 0 on a platform with no cross-process signal delivery,
    /// including this one). `key_pid == 0` marks a free slot.
    key_pid: AtomicI32,
    key_tid: AtomicI32,
    active: AtomicU32,
    sysno: AtomicU32,
    host: AtomicU32,
    /// The `Task` address, kept only so the dump can look up this thread's note.
    task: AtomicU64,
    args: [AtomicU64; 4],
    started_ms: AtomicU64,
    exit_ms: AtomicU64,
    completed: AtomicU64,
    /// `completed` as of the previous dump, so the dump can report a RATE and not just a total.
    seen_completed: AtomicU64,
    mix_sysno: [AtomicU32; STUCK_MIX],
    mix_count: [AtomicU64; STUCK_MIX],
    comm: [AtomicU64; 2],
}

static STUCK_BASE: AtomicUsize = AtomicUsize::new(0);
static STUCK_SECS: AtomicU64 = AtomicU64::new(0);
static STUCK_INIT: AtomicBool = AtomicBool::new(false);
static STUCK_LAST_DUMP_MS: AtomicU64 = AtomicU64::new(0);

/// One-shot latch for `LITEBOX_DIAG_STUCK_SECS`. Unset leaves it at 0 = off, so a run that does
/// not ask for this costs one relaxed load per dispatch and nothing else.
pub fn init_stuck(secs: impl FnOnce() -> Option<String>) {
    if STUCK_INIT.load(Ordering::Acquire) {
        return;
    }
    let v = secs()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(STUCK_DEFAULT_SECS);
    STUCK_SECS.store(v, Ordering::Release);
    STUCK_INIT.store(true, Ordering::Release);
}

pub fn stuck_enabled() -> bool {
    STUCK_SECS.load(Ordering::Acquire) != 0 && STUCK_BASE.load(Ordering::Acquire) != 0
}

/// Places the slot array in the shared arena. Called from `LinuxShimBuilder::build`, which is the
/// only place that runs once per fork family; the pointer it publishes lives in a `static`, so a
/// `fork()` child inherits the VALUE while the pages behind it stay shared.
pub fn allocate_stuck_table<Platform: litebox::platform::SharedKernelStateProvider>(
    platform: &Platform,
) {
    if STUCK_SECS.load(Ordering::Acquire) == 0 {
        return;
    }
    let layout = core::alloc::Layout::new::<[StuckSlot; STUCK_SLOTS]>();
    // `None` (arena exhausted) leaves `STUCK_BASE` at 0, which disables the whole instrument
    // rather than aborting the host process the whole guest session lives in.
    if let Some(ptr) = platform.shared_kernel_arena_alloc_bytes(layout) {
        STUCK_BASE.store(ptr.as_ptr() as usize, Ordering::Release);
    }
}

/// Arms the host thread that dumps the shared table on a clock instead of on a dispatch, so a fork
/// family in which every thread is parked at once is still observable. One second, so the
/// `LITEBOX_DIAG_STUCK_SECS` window is what throttles the output.
pub fn spawn_stuck_ticker<Platform: litebox::platform::SystemInfoProvider>(platform: &Platform) {
    if stuck_enabled() {
        platform.spawn_diag_ticker(1000, maybe_dump_stuck);
    }
}

fn stuck_base() -> Option<*const StuckSlot> {
    let p = STUCK_BASE.load(Ordering::Acquire);
    (p != 0).then_some(p as *const StuckSlot)
}

fn stuck_slot_at(base: *const StuckSlot, index: usize) -> &'static StuckSlot {
    // SAFETY: `base` is the shared arena allocation made by `allocate_stuck_table`, sized
    // `[StuckSlot; STUCK_SLOTS]`, and every process in the family maps it at this address.
    unsafe { &*base.add(index) }
}

/// Finds this thread's slot, claiming a free one when `claim`. See [`StuckSlot::key_pid`] for why
/// the key is `(pid, tid)`.
#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    reason = "the hash only picks a probe start (same arithmetic in every process of the family); pid/tid are positive"
)]
fn stuck_slot(pid: i32, tid: i32, claim: bool) -> Option<(&'static StuckSlot, bool)> {
    let base = stuck_base()?;
    let h = (pid as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (tid as u64).wrapping_mul(0x85EB);
    let start = h as usize % STUCK_SLOTS;
    for i in 0..STUCK_PROBE {
        let s = stuck_slot_at(base, (start + i) % STUCK_SLOTS);
        if s.key_pid.load(Ordering::Acquire) == pid && s.key_tid.load(Ordering::Acquire) == tid {
            return Some((s, false));
        }
    }
    if !claim {
        return None;
    }
    for i in 0..STUCK_PROBE {
        let s = stuck_slot_at(base, (start + i) % STUCK_SLOTS);
        if s.key_pid.load(Ordering::Relaxed) != 0 {
            continue;
        }
        // Published before `key_pid` turns nonzero, so a reader that matches the pid never
        // compares against a `key_tid` that is still being written.
        s.key_tid.store(tid, Ordering::Relaxed);
        if s.key_pid
            .compare_exchange(0, pid, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            for k in 0..STUCK_MIX {
                s.mix_sysno[k].store(STUCK_MIX_FREE, Ordering::Relaxed);
            }
            return Some((s, true));
        }
    }
    None
}

#[allow(
    clippy::too_many_arguments,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    reason = "one syscall entry's fields; host pid is positive; sysno fits the u32 diagnostic slot"
)]
pub fn stuck_enter(
    host: i32,
    task: u64,
    sysno: usize,
    args: [u64; 4],
    pid: i32,
    tid: i32,
    comm: &[u8],
    now_ms: u64,
) {
    if !stuck_enabled() {
        return;
    }
    let Some((s, _claimed)) = stuck_slot(pid, tid, true) else {
        return;
    };
    s.host.store(host as u32, Ordering::Relaxed);
    s.task.store(task, Ordering::Relaxed);
    s.sysno.store(sysno as u32, Ordering::Relaxed);
    for (dst, src) in s.args.iter().zip(args) {
        dst.store(src, Ordering::Relaxed);
    }
    let mut c = [0u8; 16];
    let n = comm.len().min(16);
    c[..n].copy_from_slice(&comm[..n]);
    s.comm[0].store(
        u64::from_ne_bytes(c[..8].try_into().unwrap_or([0; 8])),
        Ordering::Relaxed,
    );
    s.comm[1].store(
        u64::from_ne_bytes(c[8..].try_into().unwrap_or([0; 8])),
        Ordering::Relaxed,
    );
    s.started_ms.store(now_ms, Ordering::Relaxed);
    // Last: a reader that sees `active == 1` sees every field above already written.
    s.active.store(1, Ordering::Release);
}

pub fn stuck_exit(pid: i32, tid: i32, now_ms: u64) {
    if !stuck_enabled() {
        return;
    }
    // No slot is claimed here: `clone` rewrites `self.tid` mid-syscall, so the child's exit looks
    // up a key the entry never inserted and would only leak a second row.
    let Some((s, _claimed)) = stuck_slot(pid, tid, false) else {
        return;
    };
    s.exit_ms.store(now_ms, Ordering::Relaxed);
    s.completed.fetch_add(1, Ordering::Relaxed);
    let sysno = s.sysno.load(Ordering::Relaxed);
    let mut placed = false;
    for k in 0..STUCK_MIX {
        if s.mix_sysno[k].load(Ordering::Relaxed) == sysno {
            s.mix_count[k].fetch_add(1, Ordering::Relaxed);
            placed = true;
            break;
        }
    }
    if !placed {
        for k in 0..STUCK_MIX {
            if s.mix_sysno[k]
                .compare_exchange(STUCK_MIX_FREE, sysno, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                s.mix_count[k].fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }
    s.active.store(0, Ordering::Release);
}

fn stuck_comm(s: &StuckSlot) -> String {
    let mut c = [0u8; 16];
    c[..8].copy_from_slice(&s.comm[0].load(Ordering::Relaxed).to_ne_bytes());
    c[8..].copy_from_slice(&s.comm[1].load(Ordering::Relaxed).to_ne_bytes());
    let end = c.iter().position(|&b| b == 0).unwrap_or(16);
    String::from_utf8_lossy(&c[..end]).into_owned()
}

fn stuck_mix(s: &StuckSlot) -> String {
    let mut pairs: Vec<(u64, u32)> = Vec::new();
    for k in 0..STUCK_MIX {
        let n = s.mix_sysno[k].load(Ordering::Relaxed);
        if n != STUCK_MIX_FREE {
            pairs.push((s.mix_count[k].load(Ordering::Relaxed), n));
        }
    }
    pairs.sort_by_key(|p| core::cmp::Reverse(p.0));
    let mut out = String::new();
    for (count, n) in pairs {
        use core::fmt::Write as _;
        let _ = write!(out, " {}={}", syscall_name(n as usize), count);
    }
    out
}

/// Dumps one line per guest thread whose CURRENT syscall has been in flight for at least
/// `LITEBOX_DIAG_STUCK_SECS`, plus the busiest threads by syscalls completed since the previous
/// dump -- a spinning thread never parks, so a parked-only dump cannot see it at all.
pub fn maybe_dump_stuck(now_ms: u64) {
    if !stuck_enabled() {
        return;
    }
    let window_ms = STUCK_SECS.load(Ordering::Acquire).saturating_mul(1000);
    let last = STUCK_LAST_DUMP_MS.load(Ordering::Acquire);
    if now_ms.saturating_sub(last) < window_ms {
        return;
    }
    STUCK_LAST_DUMP_MS.store(now_ms, Ordering::Release);
    let Some(base) = stuck_base() else {
        return;
    };
    let mut parked: Vec<(u64, usize)> = Vec::new();
    let mut hot: Vec<(u64, usize)> = Vec::new();
    let mut pids: Vec<i32> = Vec::new();
    let mut in_flight = 0usize;
    for i in 0..STUCK_SLOTS {
        let s = stuck_slot_at(base, i);
        let pid = s.key_pid.load(Ordering::Acquire);
        if pid == 0 {
            continue;
        }
        if !pids.contains(&pid) {
            pids.push(pid);
        }
        let done = s.completed.load(Ordering::Relaxed);
        let since = done.saturating_sub(s.seen_completed.swap(done, Ordering::Relaxed));
        if since > 0 {
            hot.push((since, i));
        }
        if s.active.load(Ordering::Acquire) == 0 {
            continue;
        }
        in_flight += 1;
        let dur = now_ms.saturating_sub(s.started_ms.load(Ordering::Relaxed));
        if dur >= window_ms {
            parked.push((dur, i));
        }
    }
    let header = alloc::format!(
        "[diag-stuck] window_ms={} parked={} in_flight={} busy={} guest_pids={}",
        window_ms,
        parked.len(),
        in_flight,
        hot.len(),
        pids.len(),
    );
    litebox_util_log::warn!(stuck:% = header.as_str(); "[diag-stuck]");
    parked.sort_by_key(|p| core::cmp::Reverse(p.0));
    for (dur, i) in parked.iter().take(40) {
        let s = stuck_slot_at(base, *i);
        let sysno = s.sysno.load(Ordering::Relaxed) as usize;
        let line = alloc::format!(
            "stuck_ms={} pid={} tid={} comm={} syscall={}({}) a0={:#x} a1={:#x} a2={:#x} note={}",
            dur,
            s.key_pid.load(Ordering::Relaxed),
            s.key_tid.load(Ordering::Relaxed),
            stuck_comm(s),
            syscall_name(sysno),
            sysno,
            s.args[0].load(Ordering::Relaxed),
            s.args[1].load(Ordering::Relaxed),
            s.args[2].load(Ordering::Relaxed),
            thread_note(s.task.load(Ordering::Relaxed)).unwrap_or("-"),
        );
        litebox_util_log::warn!(stuck:% = line.as_str(); "[diag-stuck]");
    }
    hot.sort_by_key(|h| core::cmp::Reverse(h.0));
    for (since, i) in hot.iter().take(8) {
        let s = stuck_slot_at(base, *i);
        let line = alloc::format!(
            "since_last={} pid={} tid={} comm={} last={}({}) idle_ms={} total={} mix:{}",
            since,
            s.key_pid.load(Ordering::Relaxed),
            s.key_tid.load(Ordering::Relaxed),
            stuck_comm(s),
            syscall_name(s.sysno.load(Ordering::Relaxed) as usize),
            s.sysno.load(Ordering::Relaxed),
            now_ms.saturating_sub(s.exit_ms.load(Ordering::Relaxed)),
            s.completed.load(Ordering::Relaxed),
            stuck_mix(s),
        );
        litebox_util_log::warn!(hot:% = line.as_str(); "[diag-hot]");
    }
}

// ---------------------------------------------------------------------------
// "Did anything ever open/see this path?" (`LITEBOX_DIAG_PATH_MARKER=<substring>`)
//
// A yes/no the log could not otherwise answer: chromium under litebox names its target URL
// nowhere in its own output, so "did the renderer ever reach `file:///page.html`" was unknown.
// Matched against the path argument of every path-taking syscall, guest-side, at dispatch.
// ---------------------------------------------------------------------------

static PATH_MARKER: spin::Mutex<Option<String>> = spin::Mutex::new(None);
static PATH_MARKER_ON: AtomicBool = AtomicBool::new(false);
static PATH_MARKER_INIT: AtomicBool = AtomicBool::new(false);
static PATH_MARKER_HITS: AtomicU64 = AtomicU64::new(0);

/// One relaxed atomic load, checked on every dispatch; the string compare only happens for the
/// handful of path-taking syscall numbers the caller filters on.
pub fn path_marker_on() -> bool {
    PATH_MARKER_ON.load(Ordering::Acquire)
}

pub fn init_path_marker(value: impl FnOnce() -> Option<String>) {
    if PATH_MARKER_INIT.load(Ordering::Acquire) {
        return;
    }
    let v = value()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    PATH_MARKER_ON.store(v.is_some(), Ordering::Release);
    *PATH_MARKER.lock() = v;
    PATH_MARKER_INIT.store(true, Ordering::Release);
}

/// `path` is the NUL-terminated guest string at `ptr`.
///
/// Read ONE BYTE AT A TIME and stopped at the first NUL: guest memory is this process's memory,
/// but a bulk `from_raw_parts(ptr, 512)` can run off the end of the mapping the string lives in
/// and fault inside the shim, which would kill every guest in the process. Byte-wise reading can
/// only ever touch bytes the string itself occupies.
const PATH_MARKER_MAX_SCAN: usize = 4096;
/// Bound on emitted lines, not on matches counted.
const PATH_MARKER_MAX_HITS: u64 = 2000;

pub fn check_path_marker<Platform: litebox::platform::StdioProvider>(
    platform: &Platform,
    syscall_number: usize,
    ptr: usize,
    pid: i32,
    tid: i32,
) {
    if !PATH_MARKER_ON.load(Ordering::Acquire) {
        return;
    }
    // AT_FDCWD (-100) and friends are passed where a path pointer would be; a negative or
    // kernel-range value is never a guest string.
    // AT_FDCWD (-100) and friends are passed where a path pointer would be, and a raw `as usize`
    // of a negative i32 lands at 0x0000_ffff_ffff_ff9c -- inside the 48-bit range but outside
    // every guest user mapping. Only the canonical user half is a plausible string address.
    if !(0x1000..0x0000_8000_0000_0000).contains(&ptr) {
        return;
    }
    let guard = PATH_MARKER.lock();
    let Some(marker) = guard.as_ref() else {
        return;
    };
    let mut buf: [u8; 256] = [0; 256];
    let mut len = 0;
    // SAFETY: `ptr` is a guest user address; every byte up to the terminating NUL is part of a
    // string the guest itself just handed to this syscall, so it is mapped.
    while len < buf.len().min(PATH_MARKER_MAX_SCAN) {
        let b = unsafe { core::ptr::read_volatile((ptr + len) as *const u8) };
        if b == 0 {
            break;
        }
        buf[len] = b;
        len += 1;
    }
    let Ok(s) = core::str::from_utf8(&buf[..len]) else {
        return;
    };
    if !s.contains(marker.as_str()) {
        return;
    }
    drop(guard);
    let n = PATH_MARKER_HITS.fetch_add(1, Ordering::Relaxed) + 1;
    if n > PATH_MARKER_MAX_HITS {
        return;
    }
    emit_timeline_line(
        platform,
        &alloc::format!(
            "[diag-path] hit#{n} pid={pid} tid={tid} syscall={}({}) path={s}",
            syscall_name(syscall_number),
            syscall_number,
        ),
    );
}

/// Dumps the guest stack of one busy syscall site, so the FUNCTION THAT OWNS THE LOOP -- not just
/// the syscall it makes -- can be named. `LITEBOX_SPIN_STACK=<syscall number>`: every
/// [`SPIN_STACK_EVERY`]-th dispatch of that syscall prints 0x180 bytes from `rsp` as qwords, which
/// for a `-fstack-protector` leaf like `base::TimeTicks::Now()` (push rbp; sub $0x20) puts the
/// return address at [rsp], the saved rbp at [rsp+0x20] and ITS caller's return address at
/// [rsp+0x28] -- enough to walk the rbp chain by hand from the log.
const SPIN_STACK_EVERY: u64 = 8192;

static SPIN_STACK_SYSCALL: AtomicU64 = AtomicU64::new(u64::MAX);
static SPIN_STACK_INIT: AtomicBool = AtomicBool::new(false);
static SPIN_STACK_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn init_spin_stack(value: impl FnOnce() -> Option<String>) {
    if SPIN_STACK_INIT.load(Ordering::Acquire) {
        return;
    }
    let v = value()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(u64::MAX);
    SPIN_STACK_SYSCALL.store(v, Ordering::Release);
    SPIN_STACK_INIT.store(true, Ordering::Release);
}

pub fn maybe_dump_spin_stack<Platform: litebox::platform::StdioProvider>(
    platform: &Platform,
    syscall_number: usize,
    rsp: usize,
) {
    use core::fmt::Write as _;
    if SPIN_STACK_SYSCALL.load(Ordering::Acquire) != syscall_number as u64 {
        return;
    }
    if !SPIN_STACK_COUNT
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(SPIN_STACK_EVERY)
    {
        return;
    }
    // Guest memory is this process's memory (see the `is_syscall_trap` byte read in the platform
    // layer and the `rip` byte dump in `lib.rs`, which do the same).
    let words = unsafe { core::slice::from_raw_parts(rsp as *const u64, 0x180 / 8) };
    let mut line = alloc::format!("[diag-spin-stack] sysno={syscall_number} rsp={rsp:#x}");
    for (i, w) in words.iter().enumerate() {
        if i % 8 == 0 {
            emit_timeline_line(platform, &line);
            line = alloc::format!("[diag-spin-stack]   +{:#x}:", i * 8);
        }
        let _ = write!(line, " {w:#x}");
    }
    emit_timeline_line(platform, &line);
}

/// Emits the guest's mapping list ONCE, so a `caller=` address from the histogram can be turned
/// into a file offset (`caller - range_start`) and then into a symbol in the guest binary.
/// Without this the histogram names a syscall and an address but not the module holding it.
pub fn dump_guest_mappings_once<Platform: litebox::platform::StdioProvider>(
    platform: &Platform,
    lines: impl FnOnce() -> Vec<String>,
) {
    static DUMPED: AtomicBool = AtomicBool::new(false);
    if !rip_hist_enabled() {
        return;
    }
    if DUMPED.swap(true, Ordering::AcqRel) {
        return;
    }
    for line in lines() {
        emit_timeline_line(platform, &alloc::format!("[diag-maps] {line}"));
    }
}

/// Emits the accumulated call-site histogram and clears it, when a periodic dump has come due.
/// Called from the syscall dispatch path; a no-op (one relaxed atomic load) otherwise.
pub fn maybe_dump_rip_histogram<Platform: litebox::platform::StdioProvider>(platform: &Platform) {
    if !rip_hist_enabled() {
        return;
    }
    if !RIP_DUMP_DUE.swap(false, Ordering::AcqRel) {
        return;
    }
    let mut guard = RIP_HISTOGRAM.lock();
    let mut rows: Vec<(&(usize, u64), &RipHit)> = guard.iter().collect();
    rows.sort_by_key(|r| core::cmp::Reverse(r.1.count));
    emit_timeline_line(
        platform,
        "[diag-rip-hist] busiest syscall call sites since last dump:",
    );
    for ((num, rip), hit) in rows.iter().take(20) {
        emit_timeline_line(
            platform,
            &alloc::format!(
                "[diag-rip-hist] count={count} syscall={name}({num}) rip={rip:#x} caller={caller:#x} pid={pid} tid={tid} comm={comm}",
                count = hit.count,
                name = syscall_name(*num),
                rip = rip,
                caller = hit.caller,
                pid = hit.pid,
                tid = hit.tid,
                comm = hit.comm,
            ),
        );
    }
    guard.clear();
}

/// The sampled call-site histogram, busiest site first, for the exit-time dump.
pub fn print_rip_histogram(mut eprint: impl FnMut(&str)) {
    if !strace_summary_enabled() {
        return;
    }
    let guard = RIP_HISTOGRAM.lock();
    if guard.is_empty() {
        return;
    }
    eprint(
        "\n=== LITEBOX_STRACE_SUMMARY: syscall call sites (1-in-64 sampled rip histogram) ===\n",
    );
    let mut rows: Vec<(&(usize, u64), &RipHit)> = guard.iter().collect();
    rows.sort_by_key(|r| core::cmp::Reverse(r.1.count));
    for ((num, rip), hit) in rows.iter().take(60) {
        eprint(&alloc::format!(
            "count={count} syscall={name}({num}) rip={rip:#x} pid={pid} tid={tid} comm={comm}\n",
            count = hit.count,
            name = syscall_name(*num),
            rip = rip,
            pid = hit.pid,
            tid = hit.tid,
            comm = hit.comm,
        ));
    }
}

/// Record a raw syscall number that `SyscallRequest::try_from_raw` could not resolve at all.
pub fn record_unresolved_syscall(syscall_number: usize, pid: i32, comm: &str) {
    // These grow a `static` collection; keep its nodes in private memory so a native-`fork()`
    // child's copy of the static never aliases the parent's (see `PrivateAllocGuard`).
    let _private = litebox_util_log::PrivateAllocGuard::new();
    if !strace_summary_enabled() {
        return;
    }
    let mut guard = STRACE_SUMMARY.lock();
    guard
        .unresolved
        .entry(syscall_number)
        .or_insert_with(|| UnsupportedHit {
            first_pid: pid,
            first_comm: comm.to_string(),
            count: 0,
        })
        .count += 1;
}

/// Record a syscall/ioctl/fcntl/prctl/setsockopt sub-command that returned one of
/// [`NOTABLE_ERRNOS`]. `description` should identify the sub-command, e.g.
/// `"ioctl(0x5413)"` or `"setsockopt(SOL_SOCKET, SO_REUSEPORT)"`. `errno_name` should be the
/// `Debug`-formatted errno (e.g. `"ENOSYS"`).
pub fn record_unsupported_subcommand(description: &str, errno_name: &str, pid: i32, comm: &str) {
    // These grow a `static` collection; keep its nodes in private memory so a native-`fork()`
    // child's copy of the static never aliases the parent's (see `PrivateAllocGuard`).
    let _private = litebox_util_log::PrivateAllocGuard::new();
    if !strace_summary_enabled() {
        return;
    }
    if !NOTABLE_ERRNOS.contains(&errno_name) {
        return;
    }
    let mut guard = STRACE_SUMMARY.lock();
    let key = alloc::format!("{description} -> {errno_name}");
    guard
        .unsupported_subcommands
        .entry(key)
        .or_insert_with(|| UnsupportedHit {
            first_pid: pid,
            first_comm: comm.to_string(),
            count: 0,
        })
        .count += 1;
}

static SYSCALL_TIMELINE_ENABLED: AtomicBool = AtomicBool::new(false);
static SYSCALL_TIMELINE_INIT: AtomicBool = AtomicBool::new(false);

/// The process names the timeline is currently aimed at, parsed once from the env var's value.
///
/// Empty means the timeline is off; it is never "empty means everything", which is the whole
/// safety property here (see [`SYSCALL_TIMELINE_DEFAULT_COMMS`]).
static SYSCALL_TIMELINE_COMMS: spin::Mutex<Vec<String>> = spin::Mutex::new(Vec::new());

/// The list `LITEBOX_DIAG_SYSCALL_TIMELINE=1` selects, kept so the historical spelling of this
/// flag keeps doing exactly what it used to.
///
/// These are the XFCE components from the "why does this client go silent after CreateWindow"
/// investigation (AGENTS.md's "Rendering/scanout blocker" section), confirmed via X11 protocol
/// decode to create a window, do some property setup, then never issue another X11 request.
const SYSCALL_TIMELINE_DEFAULT_COMMS: &[&str] =
    &["xfwm4", "xfdesktop", "xfce4-panel", "xfce4-about"];

/// Call once, early, with a closure performing the platform's `env_value` lookup for
/// `LITEBOX_DIAG_SYSCALL_TIMELINE`. Idempotent, same lazy-latch pattern as
/// [`init_strace_summary`], including why the argument is a closure.
///
/// # Why this takes a value rather than a flag
///
/// The target list used to be a `const` of four hard-coded XFCE process names, deliberately
/// fixed "so this stays a targeted diagnostic rather than growing back into the every-process
/// firehose that OOM'd the host once already". The bound was right; hard-coding it was not. It
/// made the single most useful instrument in the shim answer questions about exactly one desktop
/// -- pointing it at a silent `mate-session` meant editing this file and rebuilding, which is a
/// thing you only discover you need in the middle of the investigation that needs it.
///
/// The value form keeps the bound exactly as strong: the list is still an explicit enumeration of
/// process names, still never a wildcard, and an unset variable still traces nothing. All that
/// changes is WHO writes the list -- the person running the investigation instead of whoever last
/// edited this constant.
///
/// - unset or empty -> off
/// - `1` / `true` / `yes` / `on` -> [`SYSCALL_TIMELINE_DEFAULT_COMMS`], the historical behavior
/// - anything else -> a comma-separated list of `comm` names, e.g. `mate-session,marco,caja`
pub fn init_syscall_timeline(value: impl FnOnce() -> Option<String>) {
    // These grow a `static` collection; keep its nodes in private memory so a native-`fork()`
    // child's copy of the static never aliases the parent's (see `PrivateAllocGuard`).
    let _private = litebox_util_log::PrivateAllocGuard::new();
    if SYSCALL_TIMELINE_INIT.load(Ordering::Acquire) {
        return;
    }
    let comms: Vec<String> = match value() {
        None => Vec::new(),
        Some(v) if matches!(v.trim(), "1" | "true" | "yes" | "on") => {
            SYSCALL_TIMELINE_DEFAULT_COMMS
                .iter()
                .map(|s| String::from(*s))
                .collect()
        }
        Some(v) => v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
    };
    SYSCALL_TIMELINE_ENABLED.store(!comms.is_empty(), Ordering::Release);
    *SYSCALL_TIMELINE_COMMS.lock() = comms;
    SYSCALL_TIMELINE_INIT.store(true, Ordering::Release);
}

/// Emit one already-formatted timeline line straight to the guest's stderr.
///
/// # Why not `litebox_util_log::error!`
///
/// Because that is gated on a SECOND, unrelated environment variable. The timeline's lines went
/// through the `log` macros, which reach the runner's `tracing` subscriber -- and that subscriber
/// is installed with `.with_env_var("LITEBOX_LOG")`, so with `LITEBOX_LOG` unset the level filter
/// discards every line before it is written. The result is the worst possible failure mode for an
/// instrument: `LITEBOX_DIAG_SYSCALL_TIMELINE=mate-session` is accepted, the filter matches, the
/// emit site runs -- and nothing appears. Silence from a diagnostic reads as "the thing I am
/// looking for did not happen", which is precisely the wrong conclusion, and it cost a full
/// webtop boot to find out otherwise.
///
/// A diagnostic gated by its own variable must not depend on an unrelated one to produce output.
/// The rest of this codebase already settled that: `diag_raw_print_proc_sys_open_miss` and
/// `diag_raw_print_dev_open_miss` both write to stderr through
/// [`litebox::platform::StdioProvider::write_to`], which is why THEIR output shows up
/// unconditionally. This is the same path.
///
/// Unlike those two, this one is not allocation-free -- the caller has already used `format!` to
/// build the line, and unlike an open-path miss this only ever runs from ordinary syscall
/// dispatch, where allocation is fine.
pub fn emit_timeline_line<Platform: litebox::platform::StdioProvider>(
    platform: &Platform,
    line: &str,
) {
    let _ = platform.write_to(litebox::platform::StdioOutStream::Stderr, line.as_bytes());
    let _ = platform.write_to(
        litebox::platform::StdioOutStream::Stderr,
        b"
",
    );
}

pub fn syscall_timeline_enabled() -> bool {
    SYSCALL_TIMELINE_ENABLED.load(Ordering::Acquire)
}

/// Companion pid-based target list for [`SYSCALL_TIMELINE_COMMS`] -- see
/// [`init_syscall_timeline_pids`]'s own doc comment for why the comm-based filter alone cannot
/// see a forked child's own pre-`execve` syscalls.
static SYSCALL_TIMELINE_PIDS: spin::Mutex<Vec<i32>> = spin::Mutex::new(Vec::new());
static SYSCALL_TIMELINE_PIDS_INIT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Call once, early, with a closure performing the platform's `env_value` lookup for
/// `LITEBOX_DIAG_SYSCALL_TIMELINE_PID`. Same lazy-latch shape as [`init_syscall_timeline`].
///
/// # Why a separate, pid-based filter is needed
///
/// 113th pass: a freshly `clone()`d task's `comm` is the inherited/unset value (`comm` is never
/// copied from the parent at fork time in this codebase, unlike real Linux) until that task's OWN
/// `execve` renames it -- so [`is_syscall_timeline_target_comm`]'s comm-based filter can NEVER see
/// a forked child's pre-`execve` syscalls, no matter what comm is configured, because the comm the
/// investigator actually knows (the PARENT's name) is not what the CHILD's own early syscalls carry.
/// This left a real investigation gap: a child that crashes before ever reaching its own `execve`
/// (so it never acquires ANY of the comms this diagnostic can target) is invisible to it entirely.
/// A guest pid, unlike comm, IS known and stable across a `clone()`/`execve()` boundary (it never
/// changes), and is exactly what `DIAG_TIMELINE clone`/`execve` already print -- so a pid-based
/// companion filter closes this gap while keeping the same "explicit enumeration, never a
/// wildcard" safety property `is_syscall_timeline_target_comm` already established (a bounded,
/// investigator-supplied pid list, not "every process", so the original OOM concern doesn't recur).
///
/// - unset or empty -> off (no pids traced by this filter)
/// - a comma-separated list of pids or inclusive ranges, e.g. `51,53,60-80`
pub fn init_syscall_timeline_pids(value: impl FnOnce() -> Option<String>) {
    if SYSCALL_TIMELINE_PIDS_INIT.load(Ordering::Acquire) {
        return;
    }
    let value = value();
    if value.as_deref().map(str::trim) == Some("ipc") {
        SYSCALL_TIMELINE_IPC_ALL.store(true, Ordering::Release);
        SYSCALL_TIMELINE_ENABLED.store(true, Ordering::Release);
    }
    let pids: Vec<i32> = match value {
        None => Vec::new(),
        Some(v) => v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .flat_map(|s| match s.split_once('-') {
                Some((lo, hi)) => match (lo.parse::<i32>(), hi.parse::<i32>()) {
                    (Ok(lo), Ok(hi)) if lo <= hi && hi - lo < 4096 => (lo..=hi).collect(),
                    _ => Vec::new(),
                },
                None => s.parse().ok().into_iter().collect(),
            })
            .collect(),
    };
    if !pids.is_empty() {
        SYSCALL_TIMELINE_ENABLED.store(true, Ordering::Release);
    }
    *SYSCALL_TIMELINE_PIDS.lock() = pids;
    SYSCALL_TIMELINE_PIDS_INIT.store(true, Ordering::Release);
}

static SYSCALL_TIMELINE_IPC_ALL: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// `LITEBOX_DIAG_SYSCALL_TIMELINE_PID=ipc`: every process, but only the socket, fd-plumbing and
/// process-lifetime syscalls, so a child that dies or times out before its pid is known is still
/// traced at a volume a full browser run survives.
pub fn is_ipc_timeline_syscall(number: usize) -> bool {
    SYSCALL_TIMELINE_IPC_ALL.load(Ordering::Acquire)
        && matches!(
            syscall_name_pub(number).as_str(),
            "recvmsg"
                | "sendmsg"
                | "sendto"
                | "recvfrom"
                | "socketpair"
                | "socket"
                | "connect"
                | "dup2"
                | "dup3"
                | "execve"
                | "exit_group"
                | "epoll_ctl"
                | "shutdown"
                | "clone"
                | "clone3"
                // Who is waiting for WHOM, and does the wait ever come back? A guest thread
                // parked in `wait4` for a child that will never be reported is invisible
                // everywhere else: it makes no further syscalls, so the parked instrument's
                // own rows are all it ever leaves behind.
                | "wait4"
                | "waitid"
        )
}

/// Whether `pid` is one [`init_syscall_timeline_pids`]'s own list is aimed at.
pub fn is_syscall_timeline_target_pid(pid: i32) -> bool {
    SYSCALL_TIMELINE_PIDS.lock().contains(&pid)
}

/// Whether `comm` (the raw, NUL-padded `[u8; 16]`-shaped process name, as read from
/// `Task::comm`) is one the timeline is aimed at.
///
/// A prefix match, because real Linux truncates `comm` to 15 bytes + NUL and this project's own
/// `comm` field mirrors that -- so a target name longer than 15 bytes is still matched on the
/// bytes that survive the truncation, rather than silently never matching.
pub fn is_syscall_timeline_target_comm(comm: &[u8]) -> bool {
    if !syscall_timeline_enabled() {
        return false;
    }
    let end = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
    let trimmed = &comm[..end];
    // A not-yet-`execve`'d thread's `comm` is empty (inherited/unset) -- `target.starts_with(b"")`
    // is trivially true for every target, which used to match every such thread against every
    // configured name, firing this diagnostic for the pre-exec bootstrap syscalls
    // (`set_robust_list`/`rt_sigprocmask`/`getpid`/`close`/`rt_sigaction`/...) of literally every
    // forked guest thread on the boot, not just the intended target process. Live-caught: a
    // `LITEBOX_DIAG_SYSCALL_TIMELINE=xfce4-session` run never reached `xfce4-session` at all
    // before the unrelated volume exhausted host RAM. The truncation-prefix match below only
    // ever makes sense once `execve` has actually named the process (a real Linux truncation is
    // exactly `min(15, name.len())` non-zero bytes), so require a non-empty `trimmed`.
    !trimmed.is_empty()
        && SYSCALL_TIMELINE_COMMS
            .lock()
            .iter()
            .any(|target| trimmed == target.as_bytes() || target.as_bytes().starts_with(trimmed))
}

/// Optional narrowing filter for the `litebox_diag::socket_read` payload-preview diagnostic
/// (`syscalls/file.rs`'s two `read()`/AF_UNIX `recvfrom` DIAG sites, 73rd pass). That
/// diagnostic already lives on its own dedicated tracing target (cheap when the target is not
/// enabled at all), but once a caller DOES enable it via `LITEBOX_LOG` -- which is what a real
/// X11/D-Bus wire-capture investigation needs for the whole boot -- it fires for every process
/// that reads a socket fd, not just the one process under investigation. During the
/// desktop-boot fork storm (15-22 concurrent processes, several of them touching X11/D-Bus
/// sockets) that is real, avoidable cost: each firing hex-formats up to 4096 bytes (a
/// multi-KB allocation) and pushes it through the tracing dispatcher, repeated per read, per
/// process.
///
/// 74th pass: same fix shape as [`init_syscall_timeline`]/[`is_syscall_timeline_target_comm`]
/// above (same module, same lazy-latch, same explicit-enumeration-never-wildcard safety
/// property) -- checked BEFORE the hex-formatting work happens, not after, so a caller who
/// only cares about e.g. `xfwm4`'s own reads pays nothing for every other process's socket
/// traffic. Unset (the default) leaves the diagnostic exactly as it was: gated solely by
/// whether `LITEBOX_LOG` enables the `litebox_diag::socket_read` target, for every fd on every
/// process -- this is purely an opt-in narrowing, never a behavior change for an existing
/// capture that does not set the new variable.
static SOCKET_READ_FILTER_ENABLED: AtomicBool = AtomicBool::new(false);
static SOCKET_READ_FILTER_INIT: AtomicBool = AtomicBool::new(false);
static SOCKET_READ_FILTER_COMMS: spin::Mutex<Vec<String>> = spin::Mutex::new(Vec::new());

/// Call once, early, with a closure performing the platform's `env_value` lookup for
/// `LITEBOX_DIAG_SOCKET_READ_TARGET`.
///
/// - unset or empty -> no filter: every process's socket reads are eligible (matches the
///   diagnostic's pre-74th-pass behavior exactly).
/// - anything set -> a comma-separated list of `comm` names (e.g. `xfwm4` or
///   `xfwm4,dbus-daemon`); only a matching process's socket reads emit the payload preview.
pub fn init_socket_read_filter(value: impl FnOnce() -> Option<String>) {
    // These grow a `static` collection; keep its nodes in private memory so a native-`fork()`
    // child's copy of the static never aliases the parent's (see `PrivateAllocGuard`).
    let _private = litebox_util_log::PrivateAllocGuard::new();
    if SOCKET_READ_FILTER_INIT.load(Ordering::Acquire) {
        return;
    }
    let comms: Vec<String> = match value() {
        None => Vec::new(),
        Some(v) => v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
    };
    SOCKET_READ_FILTER_ENABLED.store(!comms.is_empty(), Ordering::Release);
    *SOCKET_READ_FILTER_COMMS.lock() = comms;
    SOCKET_READ_FILTER_INIT.store(true, Ordering::Release);
}

/// Whether `comm` should have its socket-read payload preview emitted. `true` whenever no
/// filter was ever configured (the default, backward-compatible "log everyone" behavior);
/// once a filter IS configured, only a listed `comm` (same prefix-match rule as
/// [`is_syscall_timeline_target_comm`], same reason) passes.
pub fn is_socket_read_target_comm(comm: &[u8]) -> bool {
    if !SOCKET_READ_FILTER_ENABLED.load(Ordering::Acquire) {
        return true;
    }
    let end = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
    let trimmed = &comm[..end];
    SOCKET_READ_FILTER_COMMS
        .lock()
        .iter()
        .any(|target| trimmed == target.as_bytes() || target.as_bytes().starts_with(trimmed))
}

/// Public wrapper around [`syscall_name`] for `lib.rs`'s per-syscall timeline trace (the
/// original is private since it was only ever used inside this module's own summary printer
/// before now).
pub fn syscall_name_pub(number: usize) -> String {
    syscall_name(number)
}

/// How many of a task's most recent syscalls the fatal-exception trail keeps.
///
/// Chromium's `IMMEDIATE_CRASH()` is a bare `int3`, so a renderer that hits one dies on SIGTRAP
/// with no message of its own anywhere in the log -- the run's only clue about what it was
/// doing is the syscall trail leading up to it. `LITEBOX_DIAG_SYSCALL_TIMELINE` cannot answer
/// this: it has to be armed for a comm or a pid in advance, and every renderer here is a fresh
/// fork with a pid nobody could have named. 24 entries covers the whole ~0.1s of life a
/// crashing renderer gets.
const SYSCALL_TRAIL_LEN: usize = 24;

#[derive(Clone, Copy, Default)]
pub struct SyscallTrailEntry {
    pub number: usize,
    pub args: [u64; 3],
}

struct SyscallTrail {
    entries: [SyscallTrailEntry; SYSCALL_TRAIL_LEN],
    next: usize,
    filled: usize,
}

impl SyscallTrail {
    const EMPTY: SyscallTrailEntry = SyscallTrailEntry {
        number: 0,
        args: [0; 3],
    };
}

/// One per HOST PROCESS, which is the right granularity: the processes this trail is for are
/// cross-process fork children, each of which hosts exactly one guest task, so the trail is
/// unambiguously that task's. (The root process hosts many guest tasks and its trail
/// interleaves them -- read it as "what this process was doing", not "what one task did".)
static SYSCALL_TRAIL: spin::Mutex<SyscallTrail> = spin::Mutex::new(SyscallTrail {
    entries: [SyscallTrail::EMPTY; SYSCALL_TRAIL_LEN],
    next: 0,
    filled: 0,
});

/// Remembers the syscall a task just entered. Cheap enough to run unconditionally: one
/// uncontended spin-lock round trip per syscall dispatch, no allocation.
pub fn record_syscall_trail(number: usize, args: [u64; 3]) {
    let mut trail = SYSCALL_TRAIL.lock();
    let slot = trail.next;
    trail.entries[slot] = SyscallTrailEntry { number, args };
    trail.next = (slot + 1) % SYSCALL_TRAIL_LEN;
    if trail.filled < SYSCALL_TRAIL_LEN {
        trail.filled += 1;
    }
}

/// How many of a process's FIRST syscalls the child-exit diagnostic keeps.
///
/// The ring above is the right shape for a crash (what was it doing when it died) and the wrong
/// shape for a fork child's LIFE: a chromium child ends on `clock_gettime`/`futex`/`epoll_wait`,
/// which says nothing about what it did after the `fork()`. Measured on chrF44: 333
/// `Chrome_ChildIOT` children, all exit 0, whose last-24 trails contain NOT ONE `execve`,
/// `clone`, `socket`, `mmap` or `prctl` (148 had `read`, 130 `write`, 46 `sendto`). This buffer
/// stops recording once it is full, so what it holds is always the beginning.
const FIRST_TRAIL_LEN: usize = 512;

struct FirstTrail {
    entries: [SyscallTrailEntry; FIRST_TRAIL_LEN],
    next: usize,
}

impl FirstTrail {
    const EMPTY: SyscallTrailEntry = SyscallTrailEntry {
        number: 0,
        args: [0; 3],
    };
    const NEW: Self = Self {
        entries: [Self::EMPTY; FIRST_TRAIL_LEN],
        next: 0,
    };
}

/// See [`FIRST_TRAIL_LEN`]. Also one per host process, also read at a fork child's exit.
static FIRST_TRAIL: spin::Mutex<FirstTrail> = spin::Mutex::new(FirstTrail::NEW);

/// Remembers a process's earliest syscalls, until [`FIRST_TRAIL_LEN`] of them have been seen.
pub fn record_first_trail(number: usize, args: [u64; 3]) {
    let mut trail = FIRST_TRAIL.lock();
    if trail.next >= FIRST_TRAIL_LEN {
        return;
    }
    let slot = trail.next;
    trail.entries[slot] = SyscallTrailEntry { number, args };
    trail.next = slot + 1;
}

/// The recorded prefix of this process's syscall history, oldest first.
pub fn first_trail_oldest_first() -> Vec<SyscallTrailEntry> {
    let trail = FIRST_TRAIL.lock();
    trail.entries[..trail.next].to_vec()
}

/// How many of a process's LAST syscalls the last-trail child-exit diagnostic keeps.
///
/// The mirror of [`FIRST_TRAIL_LEN`]: a chromium churn child's first 512 are the zygote
/// SPECIALIZE sequence every child shares, while its last 512 are whatever it did immediately
/// before its `exit_group(0)` -- which is where "its channel answered 0 bytes" and "it shut
/// itself down" stop looking alike.
const LAST_TRAIL_LEN: usize = 512;

#[derive(Clone, Copy)]
pub struct LastTrailEntry {
    number: usize,
    args: [u64; 3],
    /// The value the guest got back in `rax`: a byte count, or `-errno` (`Errno` is encoded that
    /// way), or 0 for a channel read that hit EOF.
    ret: u64,
}

impl LastTrailEntry {
    const EMPTY: Self = Self {
        number: 0,
        args: [0; 3],
        ret: 0,
    };
}

struct LastTrail {
    entries: [LastTrailEntry; LAST_TRAIL_LEN],
    next: usize,
    filled: usize,
}

impl LastTrail {
    #[allow(
        clippy::large_stack_arrays,
        reason = "const-evaluated into the static LAST_TRAIL, never a stack frame"
    )]
    const NEW: Self = Self {
        entries: [LastTrailEntry::EMPTY; LAST_TRAIL_LEN],
        next: 0,
        filled: 0,
    };
}

/// See [`LAST_TRAIL_LEN`]. One per host process, like [`FIRST_TRAIL`], read at the same exit.
static LAST_TRAIL: spin::Mutex<LastTrail> = spin::Mutex::new(LastTrail::NEW);

/// Remembers a process's most recent syscalls, overwriting the oldest once [`LAST_TRAIL_LEN`] of
/// them have been seen. Called after dispatch, so `ret` is the value the guest saw.
///
/// Gated on the knob's budget, because unlike [`record_first_trail`] this one never fills up and
/// stops: ungated it would spin-lock and write on every syscall of every process forever, and the
/// budget is nonzero exactly when the knob is armed.
pub fn record_last_trail(number: usize, args: [u64; 3], ret: u64) {
    if CHILD_EXIT_TRAIL_LAST_BUDGET.load(Ordering::Acquire) == 0 {
        return;
    }
    let mut trail = LAST_TRAIL.lock();
    let slot = trail.next;
    trail.entries[slot] = LastTrailEntry { number, args, ret };
    trail.next = (slot + 1) % LAST_TRAIL_LEN;
    if trail.filled < LAST_TRAIL_LEN {
        trail.filled += 1;
    }
}

/// The recorded suffix of this process's syscall history, oldest first.
pub fn last_trail_oldest_first() -> Vec<LastTrailEntry> {
    let trail = LAST_TRAIL.lock();
    let start = if trail.filled == LAST_TRAIL_LEN {
        trail.next
    } else {
        0
    };
    (0..trail.filled)
        .map(|i| trail.entries[(start + i) % LAST_TRAIL_LEN])
        .collect()
}

/// The trail's entries, oldest first, for the fatal-exception diagnostic.
pub fn syscall_trail_oldest_first() -> Vec<SyscallTrailEntry> {
    let trail = SYSCALL_TRAIL.lock();
    let start = if trail.filled == SYSCALL_TRAIL_LEN {
        trail.next
    } else {
        0
    };
    (0..trail.filled)
        .map(|i| trail.entries[(start + i) % SYSCALL_TRAIL_LEN])
        .collect()
}

/// How many cross-process fork children get their exit trail dumped when
/// `LITEBOX_DIAG_CHILD_EXIT_TRAIL` is armed without an explicit count.
///
/// A cross-process fork child is its own host process, so [`SYSCALL_TRAIL`] is unambiguously that
/// child's own (see that static's comment), and the child's exit is the only moment its whole life
/// is still in the trail. That is the one shape nothing else in this file can observe: chromium's
/// ~1/s fork children exit 0 having apparently run the browser's thread-startup code without ever
/// `execve`ing, they are not named in advance (so the comm and pid filters above cannot be aimed
/// at them), and an unfiltered every-syscall timeline OOM'd the host once already. The budget
/// bounds it the same way those filters do -- an explicit count, never "every child".
const DEFAULT_CHILD_EXIT_TRAIL_BUDGET: usize = 40;

/// The child `comm` `LITEBOX_DIAG_CHILD_EXIT_TRAIL=<name>` aims at, empty when the value was a
/// count. A boot spends its first few hundred forks on `sh`/`python3`/`Xvfb`, so a plain count
/// spends its whole budget long before the process under investigation ever forks; naming the
/// comm skips straight to it. Same prefix rule as [`is_syscall_timeline_target_comm`], for the
/// same 15-byte-`comm`-truncation reason.
static CHILD_EXIT_TRAIL_COMM: spin::Mutex<Vec<u8>> = spin::Mutex::new(Vec::new());

/// The cap a comm-selected trail dump still runs under -- "every `chromium` child" is not
/// unboundedly many, for the same reason an unfiltered every-syscall timeline is not.
const CHILD_EXIT_TRAIL_COMM_BUDGET: usize = 4096;

static CHILD_EXIT_TRAIL_BUDGET: AtomicUsize = AtomicUsize::new(0);
static CHILD_EXIT_TRAIL_INIT: AtomicBool = AtomicBool::new(false);

/// Dumps the trail of a cross-process fork child that is about to exit, called from the generic
/// syscall-entry hook rather than from the shim's own `sys_exit`/`sys_exit_group`.
///
/// WHY A SECOND ENTRY POINT: those two call sites are gated on
/// `syscalls::process::IS_NATIVE_FORK_CHILD`, which only the in-place-resume branch of a native
/// fork sets. The children a real workload actually produces are not that branch -- they are
/// `CreateProcessW`-spawned re-execs of the runner that resume a rebuilt `Task` from a register
/// snapshot (the runner's `task-resume-probe` path), they never re-run the `clone` call stack, and
/// their guest exit is encoded and ended by the runner. Measured: 649 chromium children and 4
/// python children, every one armed with a budget of 40, produced ZERO trails through the
/// `IS_NATIVE_FORK_CHILD` sites.
///
/// So this one identifies a fork child the way the child itself does -- the presence of the
/// register snapshot in its own environment (`LITEBOX_INTERNAL_FORK_CHILD_GPRS`, read once) --
/// and fires on the `exit`/`exit_group` SYSCALL NUMBER, before dispatch, so it cannot be bypassed
/// by whatever the exit path turns out to be.
#[allow(
    clippy::cast_possible_truncation,
    reason = "the exit status is the guest's i32 exit code; truncating args[0] is the existing semantics"
)]
pub fn maybe_dump_child_exit_trail<Platform>(
    platform: &Platform,
    syscall_number: usize,
    pid: i32,
    tid: i32,
    comm: &[u8],
    args: [u64; 3],
) where
    Platform: litebox::platform::StdioProvider + litebox::platform::SystemInfoProvider,
{
    if !matches!(
        ::syscalls::Sysno::new(syscall_number),
        Some(::syscalls::Sysno::exit | ::syscalls::Sysno::exit_group)
    ) {
        return;
    }
    let first_armed = !(CHILD_EXIT_TRAIL_BUDGET.load(Ordering::Acquire) == 0
        && CHILD_EXIT_TRAIL_COMM.lock().is_empty());
    if !first_armed && !child_exit_last_trail_armed() {
        return;
    }
    if !IS_FORK_CHILD_HOST.load(Ordering::Acquire) {
        if platform
            .env_value("LITEBOX_INTERNAL_FORK_CHILD_GPRS")
            .is_none()
        {
            return;
        }
        IS_FORK_CHILD_HOST.store(true, Ordering::Release);
    }
    if first_armed {
        emit_child_exit_trail(platform, pid, tid, args[0] as i32, comm, "hook");
    }
    emit_child_exit_last_trail(platform, pid, tid, args[0] as i32, comm, "hook");
}

/// True once this host process has been seen to carry the cross-process fork child's register
/// snapshot environment variable -- see [`maybe_dump_child_exit_trail`] for why that, and not
/// `IS_NATIVE_FORK_CHILD`, is what makes a process a fork child here.
static IS_FORK_CHILD_HOST: AtomicBool = AtomicBool::new(false);

/// One trail per child, whichever exit site gets there first -- a child's `sys_exit_group` and its
/// later `prepare_for_exit` would otherwise dump the same life twice.
static CHILD_EXIT_TRAIL_DUMPED: AtomicBool = AtomicBool::new(false);

/// The `LITEBOX_DIAG_CHILD_EXIT_TRAIL_LAST` knob's own latch, budget, comm filter and one-dump
/// flag: the two knobs are independent, so a child is worth its last syscalls whether or not its
/// first ones were asked for.
static CHILD_EXIT_TRAIL_LAST_COMM: spin::Mutex<Vec<u8>> = spin::Mutex::new(Vec::new());
static CHILD_EXIT_TRAIL_LAST_BUDGET: AtomicUsize = AtomicUsize::new(0);
static CHILD_EXIT_TRAIL_LAST_INIT: AtomicBool = AtomicBool::new(false);
static CHILD_EXIT_TRAIL_LAST_DUMPED: AtomicBool = AtomicBool::new(false);

/// Whether the last-trail knob is armed in this host process. Read on every syscall by
/// [`record_last_trail`], so it stays one atomic load in the common case.
pub fn child_exit_last_trail_armed() -> bool {
    !(CHILD_EXIT_TRAIL_LAST_BUDGET.load(Ordering::Acquire) == 0
        && CHILD_EXIT_TRAIL_LAST_COMM.lock().is_empty())
}

/// Parses the value grammar the per-child diagnostics share -- `<n>`, `<comm>` or `<comm>:<n>`,
/// plus the `1`/`true`/`yes`/`on` spellings of "on" -- into a child budget and a comm prefix
/// (empty means "any child").
fn parse_child_trail_target(
    value: Option<String>,
    default_budget: usize,
    comm_budget: usize,
) -> (usize, Vec<u8>) {
    match value {
        None => (0, Vec::new()),
        Some(v) if matches!(v.trim(), "1" | "true" | "yes" | "on") => (default_budget, Vec::new()),
        Some(v) => {
            let v = v.trim();
            match v.parse::<usize>() {
                Ok(n) => (n, Vec::new()),
                // `<comm>` or `<comm>:<count>` -- the count is what bounds a comm that matches
                // hundreds of children (chromium's churn), since the default comm budget does not.
                Err(_) => match v.rsplit_once(':') {
                    Some((comm, n)) if !comm.is_empty() => (
                        n.trim().parse::<usize>().unwrap_or(comm_budget),
                        comm.as_bytes().to_vec(),
                    ),
                    _ => (comm_budget, v.as_bytes().to_vec()),
                },
            }
        }
    }
}

/// Call once, early, with a closure performing the platform's `env_value` lookup for
/// `LITEBOX_DIAG_CHILD_EXIT_TRAIL`. Idempotent, same lazy-latch pattern as
/// [`init_syscall_timeline`], including why the argument is a closure.
///
/// - unset or empty -> off
/// - `1` / `true` / `yes` / `on` -> [`DEFAULT_CHILD_EXIT_TRAIL_BUDGET`] children
/// - a number -> that many children (any child, in exit order)
/// - `<comm>` -> that `comm`, under [`CHILD_EXIT_TRAIL_COMM_BUDGET`]
/// - `<comm>:<count>` -> that `comm`, at most `count` of them
///
/// Prints one `[diag-child-exit-init]` line per host process when it arms itself, so a run that
/// produces no trails can be read as "the child never saw this variable" rather than "the child
/// never exited". Silent when unarmed, so an ordinary run pays nothing for it.
pub fn init_child_exit_trail<Platform: litebox::platform::StdioProvider>(
    value: impl FnOnce() -> Option<String>,
    platform: &Platform,
) {
    // This one DOES allocate (the comm is kept as bytes); `PrivateAllocGuard` keeps its nodes in
    // private memory so a native-`fork()` child's copy of the static never aliases the parent's.
    let _private = litebox_util_log::PrivateAllocGuard::new();
    if CHILD_EXIT_TRAIL_INIT.load(Ordering::Acquire) {
        return;
    }
    let (budget, comm) = parse_child_trail_target(
        value(),
        DEFAULT_CHILD_EXIT_TRAIL_BUDGET,
        CHILD_EXIT_TRAIL_COMM_BUDGET,
    );
    *CHILD_EXIT_TRAIL_COMM.lock() = comm;
    CHILD_EXIT_TRAIL_BUDGET.store(budget, Ordering::Release);
    CHILD_EXIT_TRAIL_INIT.store(true, Ordering::Release);
    if budget != 0 {
        let comm = String::from_utf8_lossy(&CHILD_EXIT_TRAIL_COMM.lock()).into_owned();
        let _ = platform.write_to(
            litebox::platform::StdioOutStream::Stderr,
            alloc::format!("[diag-child-exit-init] budget={budget} comm={comm}\n").as_bytes(),
        );
    }
}

/// Arms the LAST-syscalls variant, from `LITEBOX_DIAG_CHILD_EXIT_TRAIL_LAST`. Same value grammar
/// as [`init_child_exit_trail`] (`<n>`, `<comm>` or `<comm>:<n>`), read through the same parser,
/// and prints one `[diag-child-exit-init] trail=last` line per host process for the same reason:
/// so a run with no last-trails reads as "the child never saw this variable" rather than "the
/// child never exited".
///
/// Independent of the other knob: either may be set alone, and setting both prints both trails.
pub fn init_child_exit_last_trail<Platform: litebox::platform::StdioProvider>(
    value: impl FnOnce() -> Option<String>,
    platform: &Platform,
) {
    let _private = litebox_util_log::PrivateAllocGuard::new();
    if CHILD_EXIT_TRAIL_LAST_INIT.load(Ordering::Acquire) {
        return;
    }
    let (budget, comm) = parse_child_trail_target(
        value(),
        DEFAULT_CHILD_EXIT_TRAIL_BUDGET,
        CHILD_EXIT_TRAIL_COMM_BUDGET,
    );
    *CHILD_EXIT_TRAIL_LAST_COMM.lock() = comm;
    CHILD_EXIT_TRAIL_LAST_BUDGET.store(budget, Ordering::Release);
    CHILD_EXIT_TRAIL_LAST_INIT.store(true, Ordering::Release);
    if budget != 0 {
        let comm = String::from_utf8_lossy(&CHILD_EXIT_TRAIL_LAST_COMM.lock()).into_owned();
        let _ = platform.write_to(
            litebox::platform::StdioOutStream::Stderr,
            alloc::format!("[diag-child-exit-init] trail=last budget={budget} comm={comm}\n")
                .as_bytes(),
        );
    }
}

/// Prints this task's syscall trail to stderr, if [`init_child_exit_trail`] armed this diagnostic
/// and a budget slot is still available.
///
/// Straight to stderr through the platform's stdio, not through `litebox_util_log`, for exactly
/// the reason [`emit_timeline_line`] documents: an instrument gated by its own environment
/// variable must not go silent because an unrelated one filtered its lines away.
pub fn emit_child_exit_trail<Platform: litebox::platform::StdioProvider>(
    platform: &Platform,
    pid: i32,
    tid: i32,
    status: i32,
    comm: &[u8],
    site: &str,
) {
    let end = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
    let trimmed = &comm[..end];
    let wanted = CHILD_EXIT_TRAIL_COMM.lock();
    let comm_ok = wanted.is_empty() || trimmed == wanted.as_slice() || wanted.starts_with(trimmed);
    let budget = CHILD_EXIT_TRAIL_BUDGET.load(Ordering::Acquire);
    let entries = first_trail_oldest_first();
    // One header per call, emitted BEFORE every gate on purpose: a header with `budget=0` means
    // the variable never reached this child, `comm_ok=false` means the comm filter rejected it,
    // `entries=0` means the trail was empty, and NO header at all means this child's exit never
    // reached this function -- four different-looking "the instrument printed nothing" failures
    // that are otherwise indistinguishable from the log.
    let _ = platform.write_to(
        litebox::platform::StdioOutStream::Stderr,
        alloc::format!(
            "[diag-child-exit] site={site} pid={pid} tid={tid} comm={} status={status} budget={budget} comm_ok={comm_ok} entries={}\n",
            String::from_utf8_lossy(trimmed),
            entries.len(),
        )
        .as_bytes(),
    );
    if !comm_ok {
        return;
    }
    if CHILD_EXIT_TRAIL_DUMPED.swap(true, Ordering::AcqRel) {
        return;
    }
    let mut budget = budget;
    while budget != 0 {
        match CHILD_EXIT_TRAIL_BUDGET.compare_exchange_weak(
            budget,
            budget - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(current) => budget = current,
        }
    }
    if budget == 0 {
        return;
    }
    let comm = String::from_utf8_lossy(trimmed);
    for (i, entry) in entries.iter().enumerate() {
        let _ = platform.write_to(
            litebox::platform::StdioOutStream::Stderr,
            alloc::format!(
                "[diag-child-exit] pid={pid} tid={tid} comm={comm} status={status} i={i} syscall={} arg0={:#x} arg1={:#x} arg2={:#x}\n",
                syscall_name(entry.number),
                entry.args[0],
                entry.args[1],
                entry.args[2],
            )
            .as_bytes(),
        );
    }
}

/// Takes one child slot out of `slot`, false once the budget is spent.
fn spend_child_trail_budget(slot: &AtomicUsize) -> bool {
    let mut budget = slot.load(Ordering::Acquire);
    while budget != 0 {
        match slot.compare_exchange_weak(budget, budget - 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(current) => budget = current,
        }
    }
    false
}

/// Prints this task's LAST syscalls to stderr, each one with the value the guest got back, if
/// `LITEBOX_DIAG_CHILD_EXIT_TRAIL_LAST` armed this diagnostic and a budget slot is still
/// available. Completely inert when that knob is unset, so a run that armed only the FIRST trail
/// is byte-identical to what it printed before this existed.
///
/// THE RETURN VALUE IS THE POINT: `read(21, buf, 8)` is one line whether it got 8 bytes, hit EOF
/// or got `-ECONNRESET`, and those are three different answers to "why did this child stop".
/// Same [`emit_child_exit_trail`] header convention plus `trail=last`, so the two trails stay
/// separable in a log where both knobs are set.
pub fn emit_child_exit_last_trail<Platform: litebox::platform::StdioProvider>(
    platform: &Platform,
    pid: i32,
    tid: i32,
    status: i32,
    comm: &[u8],
    site: &str,
) {
    if !child_exit_last_trail_armed() {
        return;
    }
    let end = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
    let trimmed = &comm[..end];
    let wanted = CHILD_EXIT_TRAIL_LAST_COMM.lock();
    let comm_ok = wanted.is_empty() || trimmed == wanted.as_slice() || wanted.starts_with(trimmed);
    let budget = CHILD_EXIT_TRAIL_LAST_BUDGET.load(Ordering::Acquire);
    let entries = last_trail_oldest_first();
    let _ = platform.write_to(
        litebox::platform::StdioOutStream::Stderr,
        alloc::format!(
            "[diag-child-exit] trail=last site={site} pid={pid} tid={tid} comm={} status={status} budget={budget} comm_ok={comm_ok} entries={}\n",
            String::from_utf8_lossy(trimmed),
            entries.len(),
        )
        .as_bytes(),
    );
    if !comm_ok {
        return;
    }
    if CHILD_EXIT_TRAIL_LAST_DUMPED.swap(true, Ordering::AcqRel) {
        return;
    }
    if !spend_child_trail_budget(&CHILD_EXIT_TRAIL_LAST_BUDGET) {
        return;
    }
    let comm = String::from_utf8_lossy(trimmed);
    for (i, entry) in entries.iter().enumerate() {
        let signed = entry.ret.cast_signed();
        let _ = platform.write_to(
            litebox::platform::StdioOutStream::Stderr,
            alloc::format!(
                "[diag-child-exit] trail=last pid={pid} tid={tid} comm={comm} status={status} i={i} syscall={} arg0={:#x} arg1={:#x} arg2={:#x} ret={signed}\n",
                syscall_name(entry.number),
                entry.args[0],
                entry.args[1],
                entry.args[2],
            )
            .as_bytes(),
        );
    }
}

/// How many IPC lines `LITEBOX_DIAG_CHILD_IPC` prints per child host process.
const CHILD_IPC_DEFAULT_BUDGET: usize = 200;

static CHILD_IPC_BUDGET: AtomicUsize = AtomicUsize::new(0);
static CHILD_IPC_INIT: AtomicBool = AtomicBool::new(false);
static CHILD_IPC_COMM: spin::Mutex<Vec<u8>> = spin::Mutex::new(Vec::new());

/// Arms the fork-child IPC tracer, same lazy-latch and value grammar as
/// [`init_child_exit_trail`] (`<n>`, `<comm>` or `<comm>:<n>`), and prints one
/// `[diag-child-ipc-init]` line per host process so a silent instrument is distinguishable from a
/// child that never made an IPC syscall.
#[allow(
    clippy::similar_names,
    reason = "pid/ppid are the kernel's own names and the edge set is printed as ppid=/pid="
)]
pub fn init_child_ipc<Platform: litebox::platform::StdioProvider>(
    value: impl FnOnce() -> Option<String>,
    platform: &Platform,
    pid: i32,
    ppid: i32,
    _comm: &[u8],
) {
    let _private = litebox_util_log::PrivateAllocGuard::new();
    if CHILD_IPC_INIT.load(Ordering::Acquire) {
        return;
    }
    let (budget, comm) =
        parse_child_trail_target(value(), CHILD_IPC_DEFAULT_BUDGET, CHILD_IPC_DEFAULT_BUDGET);
    let end = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
    let own = String::from_utf8_lossy(&comm[..end]).into_owned();
    *CHILD_IPC_COMM.lock() = comm;
    CHILD_IPC_BUDGET.store(budget, Ordering::Release);
    CHILD_IPC_INIT.store(true, Ordering::Release);
    if budget != 0 {
        // `ppid` is here because "which process is forking 700 times a minute" is the question
        // this instrument exists to answer, and a child's own pid cannot name its parent.
        let _ = platform.write_to(
            litebox::platform::StdioOutStream::Stderr,
            alloc::format!(
                "[diag-child-ipc-init] pid={pid} ppid={ppid} budget={budget} comm={own}\n"
            )
            .as_bytes(),
        );
    }
}

/// Prints one line per IPC-shaped syscall a cross-process fork child makes, WITH ITS RETURN VALUE.
///
/// A trail of arguments alone cannot answer the question the churn poses: `read(21, buf, 4)` looks
/// identical whether it got its 4 bytes, got EOF, or got `-ECONNRESET`, and a child whose command
/// channel answered 0 bytes explains itself (`exit 0` right after) while one that got 4 does not.
/// Called after dispatch, so `ret` is the value the guest actually sees: positive for a byte count,
/// `-errno` for a failure (`Errno` is encoded that way into `rax`).
///
/// Restricted to a fork child (same `LITEBOX_INTERNAL_FORK_CHILD_GPRS` test
/// [`maybe_dump_child_exit_trail`] uses) and to syscalls that carry a channel: the root process
/// makes thousands of these per second and would drown the log.
pub fn maybe_trace_child_ipc<Platform>(
    platform: &Platform,
    syscall_number: usize,
    args: [u64; 3],
    ret: u64,
    pid: i32,
    tid: i32,
    comm: &[u8],
) where
    Platform: litebox::platform::StdioProvider + litebox::platform::SystemInfoProvider,
{
    if CHILD_IPC_BUDGET.load(Ordering::Acquire) == 0 {
        return;
    }
    if !matches!(
        ::syscalls::Sysno::new(syscall_number),
        Some(
            ::syscalls::Sysno::read
                | ::syscalls::Sysno::readv
                | ::syscalls::Sysno::recvmsg
                | ::syscalls::Sysno::recvfrom
                | ::syscalls::Sysno::write
                | ::syscalls::Sysno::writev
                | ::syscalls::Sysno::sendto
                | ::syscalls::Sysno::sendmsg
                | ::syscalls::Sysno::socket
                | ::syscalls::Sysno::socketpair
                | ::syscalls::Sysno::connect
                | ::syscalls::Sysno::bind
                | ::syscalls::Sysno::execve
        )
    ) {
        return;
    }
    let end = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
    let trimmed = &comm[..end];
    let wanted = CHILD_IPC_COMM.lock();
    if !wanted.is_empty() && trimmed != wanted.as_slice() && !wanted.starts_with(trimmed) {
        return;
    }
    if !IS_FORK_CHILD_HOST.load(Ordering::Acquire) {
        if platform
            .env_value("LITEBOX_INTERNAL_FORK_CHILD_GPRS")
            .is_none()
        {
            return;
        }
        IS_FORK_CHILD_HOST.store(true, Ordering::Release);
    }
    let mut budget = CHILD_IPC_BUDGET.load(Ordering::Acquire);
    while budget != 0 {
        match CHILD_IPC_BUDGET.compare_exchange_weak(
            budget,
            budget - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(current) => budget = current,
        }
    }
    if budget == 0 {
        return;
    }
    let signed = ret.cast_signed();
    let _ = platform.write_to(
        litebox::platform::StdioOutStream::Stderr,
        alloc::format!(
            "[diag-child-ipc] pid={pid} tid={tid} comm={} syscall={} arg0={:#x} arg1={:#x} arg2={:#x} ret={signed}\n",
            String::from_utf8_lossy(trimmed),
            syscall_name(syscall_number),
            args[0],
            args[1],
            args[2],
        )
        .as_bytes(),
    );
}

fn syscall_name(number: usize) -> String {
    #[cfg(target_arch = "x86_64")]
    {
        ::syscalls::Sysno::new(number).map_or_else(
            || alloc::format!("sysno_{number}"),
            |s| s.name().to_string(),
        )
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        alloc::format!("sysno_{number}")
    }
}

/// Prints the accumulated per-syscall summary to stderr via the platform's stdio, if enabled.
/// Called once at runner exit.
pub fn print_strace_summary(mut eprint: impl FnMut(&str)) {
    use core::fmt::Write as _;
    if !strace_summary_enabled() {
        return;
    }
    let guard = STRACE_SUMMARY.lock();

    eprint("\n=== LITEBOX_STRACE_SUMMARY: per-syscall table ===\n");
    eprint("syscall(number)                     count    total_ns      max_ns  errors\n");
    let mut rows: Vec<(&usize, &SyscallStats)> = guard.by_syscall.iter().collect();
    rows.sort_by_key(|r| core::cmp::Reverse(r.1.total_ns));
    for (num, stats) in rows {
        let name = syscall_name(*num);
        let mut errs = String::new();
        for (errno, count) in &stats.errors {
            if !errs.is_empty() {
                errs.push_str(", ");
            }
            let _ = write!(errs, "{errno}={count}");
        }
        eprint(&alloc::format!(
            "{name}({num})  count={count} total_ns={total_ns} max_ns={max_ns} errors=[{errs}]\n",
            count = stats.count,
            total_ns = stats.total_ns,
            max_ns = stats.max_ns,
        ));
    }

    if !guard.unresolved.is_empty() {
        eprint("\n=== LITEBOX_STRACE_SUMMARY: unresolved syscall numbers ===\n");
        for (num, hit) in &guard.unresolved {
            eprint(&alloc::format!(
                "syscall_number={num} count={count} first_caller={comm}[{pid}]\n",
                count = hit.count,
                comm = hit.first_comm,
                pid = hit.first_pid,
            ));
        }
    }

    eprint(
        "\n=== LITEBOX_STRACE_SUMMARY: unsupported sub-commands (ENOSYS/EINVAL/ENOTSUP/EOPNOTSUPP/EPERM) ===\n",
    );
    for (desc, hit) in &guard.unsupported_subcommands {
        eprint(&alloc::format!(
            "{desc} count={count} first_caller={comm}[{pid}]\n",
            count = hit.count,
            comm = hit.first_comm,
            pid = hit.first_pid,
        ));
    }

    print_rip_histogram(eprint);
}

// ---------------------------------------------------------------------------------------------
// Process timeline + tree (always-on; item 3 of the advisor-db diagnostics spec).
// ---------------------------------------------------------------------------------------------

struct ProcessTreeEntry {
    ppid: i32,
    comm: String,
}

static PROCESS_TREE: spin::Mutex<BTreeMap<i32, ProcessTreeEntry>> =
    spin::Mutex::new(BTreeMap::new());
static PROCESS_SEQ: AtomicU64 = AtomicU64::new(0);

/// Returns a monotonically increasing sequence number for ordering timeline events relative to
/// each other (this crate has no `no_std`-safe way to capture a process-start-relative
/// timestamp without threading a `Platform`-generic `Instant` through this module -- see
/// `diag.rs`'s module doc comment; a sequence number gives the same relative-ordering value for
/// the "time since process start" field the spec asks for).
pub fn next_seq() -> u64 {
    PROCESS_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Record (or update) a pid -> (ppid, comm) edge for the exit-time process-tree dump.
#[allow(
    clippy::similar_names,
    reason = "pid/ppid are the kernel's own names and the edge set is printed as ppid=/pid="
)]
pub fn record_process(pid: i32, ppid: i32, comm: &str) {
    // These grow a `static` collection; keep its nodes in private memory so a native-`fork()`
    // child's copy of the static never aliases the parent's (see `PrivateAllocGuard`).
    let _private = litebox_util_log::PrivateAllocGuard::new();
    let mut guard = PROCESS_TREE.lock();
    guard.insert(
        pid,
        ProcessTreeEntry {
            ppid,
            comm: comm.to_string(),
        },
    );
}

/// Prints the pid -> ppid process tree built from every `record_process` call this run, at
/// runner exit.
pub fn print_process_tree(mut eprint: impl FnMut(&str)) {
    let guard = PROCESS_TREE.lock();
    if guard.is_empty() {
        return;
    }
    eprint("\n=== LITEBOX process tree (pid -> ppid, comm) ===\n");
    for (pid, entry) in guard.iter() {
        eprint(&alloc::format!(
            "pid={pid} ppid={ppid} comm={comm}\n",
            ppid = entry.ppid,
            comm = entry.comm,
        ));
    }
}
