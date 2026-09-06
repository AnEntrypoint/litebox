//! Cross-process synchronization primitives for Windows.
//!
//! # Why this exists
//!
//! `ADVISORY-002-d-zero-fork.md` §3.2 establishes that litebox's eventual `D == 0`
//! fork model (each guest "process" as its own real Windows process, sharing
//! kernel-equivalent state through a process-shared section) needs a lock that is
//! correct across a *real* Windows process boundary. The existing
//! [`crate::RawMutex`] is not that lock, and cannot be made into it:
//!
//! **Every native address- or TID-based wait on Windows is process-local by
//! design.** This is a documented property of all the obvious candidates, not an
//! oversight to route around:
//!
//! - `WaitOnAddress`/`WakeByAddressSingle` -- MSDN specifies the waking thread must
//!   be "another thread **in the same process**". This is what the existing
//!   `RawMutex` is built on (`lib.rs`, `WaitOnAddress` / `WakeByAddressSingle`),
//!   which is precisely why it does not survive the process boundary.
//! - `NtWaitForKeyedEvent`/`NtReleaseKeyedEvent` -- the released thread "must be
//!   within the same process as the signaling". Confirmed empirically on this exact
//!   host by `advisor/probes/clone_probe.c`: both sides time out.
//! - `NtAlertThreadByThreadId`/`NtWaitForAlertByThreadId` -- takes a bare TID, but
//!   returns `STATUS_ACCESS_DENIED` cross-process. Since Win8 these are what SRWLock,
//!   condition variables and `WaitOnAddress` are themselves built on, which is the
//!   underlying reason the first bullet holds.
//!
//! So the only cross-process wake is **through a shared kernel object**, and the
//! design is therefore forced rather than chosen.
//!
//! # The design
//!
//! A hybrid: a shared *state word* living inside the process-shared section for the
//! uncontended fast path, plus a genuine kernel `Event` object used only when the
//! lock is actually contended.
//!
//! ```text
//!   shared section (mapped in every participating process)
//!   +--------------------------------------------------+
//!   | CrossProcessMutex { state: AtomicU32 }            |   <- POD, repr(C)
//!   +--------------------------------------------------+
//!
//!   per-process, NOT in the section
//!   +--------------------------------------------------+
//!   | HANDLE to the auto-reset Event, opened by name    |
//!   +--------------------------------------------------+
//! ```
//!
//! The state word uses the classic three-state futex encoding
//! ([`FREE`]/[`LOCKED`]/[`CONTENDED`]). The uncontended acquire and release are a
//! single atomic each, with no kernel transition at all; only a genuinely blocked
//! waiter pays for `WaitForSingleObject`.
//!
//! ## Two properties the state word must have, and does
//!
//! 1. **It is plain POD with no self-referential encoding.** Measured directly by
//!    `advisor/probes/xproc_mutex_probe.c`: the same section maps at *different*
//!    virtual addresses in each process (e.g. `0x2330_4460_0000` in the parent vs
//!    `0x18B1_E6B0_0000` in the child, on this host). Anything keyed by the slot's
//!    own address would therefore be wrong in the peer -- which is exactly the
//!    failure class `ADVISORY-001` §3N root-caused in glibc's safe-linked tcache.
//!    A bare `AtomicU32` has no such dependence.
//!
//! 2. **No `Drop`, no pointers, no vtables.** The struct is `repr(C)` with a single
//!    `AtomicU32` field, so it can be placed at a raw offset inside a shared mapping
//!    and interpreted identically by every process, regardless of where that mapping
//!    landed.
//!
//! ## Why a *named* Event rather than a duplicated per-waiter handle
//!
//! `ADVISORY-002` §3.2 sketched "a per-waiter auto-reset `Event` whose handle is
//! duplicated into the waker's process on demand and cached per `(process, waiter)`".
//! That shape works, but it requires the releaser to know which process owns each
//! waiter, which means a registry of live process handles inside the shared section
//! and a `DuplicateHandle` round trip on the wake path -- substantial machinery, and
//! a nontrivial lifetime problem when a waiter's process dies while holding an entry.
//!
//! A single named auto-reset Event per mutex avoids all of it: each process opens
//! the *same* kernel object by name with `CreateEventW` (which opens rather than
//! creates when the name already exists), so no handle ever has to cross the
//! boundary. The cost is that a release wakes one arbitrary waiter rather than a
//! specific one, which for a mutex is exactly the desired semantics anyway.
//!
//! Two consequences are worth stating plainly rather than leaving implicit:
//!
//! - A wake can be *spurious* (the woken waiter loses the ensuing race to another
//!   thread). The acquire loop re-checks the state word after every wake, so this is
//!   correct, merely slightly wasteful.
//! - Because a release only signals when it observes [`CONTENDED`], and a waiter only
//!   sleeps after unconditionally *writing* [`CONTENDED`], there is no window in
//!   which a releaser sees no waiter while a waiter is about to sleep. This is the
//!   lost-wakeup argument, spelled out at [`CrossProcessMutex::lock`].
//!
//! # Known limitation: this lock is not robust to holder death
//!
//! Stated plainly rather than left to be discovered later. If a process dies while
//! holding the lock, the state word stays `LOCKED`/`CONTENDED` forever and every
//! other participant blocks indefinitely. This is the same behaviour as a POSIX
//! non-robust `pthread_mutex_t` in shared memory, and unlike a Windows kernel `Mutex`
//! object, which reports `WAIT_ABANDONED` in exactly this case.
//!
//! That trade is deliberate. A kernel `Mutex` is the other correct cross-process
//! design and it *does* give abandonment detection for free, but it was measured on
//! this host at roughly 840 ns per uncontended acquire/release pair against roughly
//! 5 ns for this hybrid, i.e. about 170x slower, because every acquire and every
//! release round-trips through the kernel. For litebox's use, guarding small
//! kernel-equivalent state under a fork model where an unexpectedly dead guest
//! process is already a fatal condition for the sandbox, blocking forever is not
//! meaningfully worse than aborting.
//!
//! If robustness is later required, the shape is: store the owner's PID alongside the
//! state word, and on a contended acquire that has waited past some bound, open the
//! owner process and check whether it has exited before force-releasing. Do not add
//! it speculatively; it puts a syscall on a path that currently has none.
//!
//! # Verification
//!
//! This protocol was verified live, before it was written in Rust, by
//! `advisor/probes/xproc_mutex_probe.c` -- two genuinely separate Windows processes
//! (ordinary `CreateProcessW`), a pagefile-backed section mapped into both, and 8-16
//! threads in *each* process hammering a single lock-protected counter with a
//! deliberately non-atomic read-modify-write. Two independent invariants are checked:
//! the final counter must be exactly `procs * threads * iters`, and a "critical
//! section occupancy" word incremented on entry must be observed as exactly 1 by
//! every holder (which catches real mutual-exclusion violations that a
//! counter-only check can mask). Across 25+ runs and three contention regimes --
//! including one with the spin disabled entirely, forcing 14,114 real
//! `WaitForSingleObject` blocks, every run passed with zero violations and zero
//! lost wakeups.
//!
//! The Rust implementation in this file was then verified the same way by
//! `advisor/probes/xproc_rust_probe/` (43 consecutive passing runs, up to 24 threads
//! per process, including a deliberately non-atomic read-modify-write through a raw
//! pointer inside the critical section, which totals correctly only if the lock
//! genuinely excludes across the process boundary).
//!
//! Finally, `advisor/probes/xproc_mutex_clone_probe.c` verifies the protocol under
//! `RtlCloneUserProcess` specifically, which is the mechanism Track B step 5 actually
//! intends to use, and where the child inherits the address space copy-on-write
//! rather than mapping the section by name. 12 consecutive passing runs, with 333
//! real cross-process Event wakeups in a representative run. That the shared counter
//! reaches its full expected total there is itself the proof that the pre-clone
//! section view stayed genuinely *shared* rather than being privatised by
//! copy-on-write: had it been privatised, each process would have counted only its
//! own half.

use core::sync::atomic::{AtomicU32, Ordering};
use std::io;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_TIMEOUT, GetLastError, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, SetEvent, WaitForSingleObject,
};

/// Lock is unheld.
const FREE: u32 = 0;
/// Lock is held, and no thread is known to be blocked on the kernel event.
const LOCKED: u32 = 1;
/// Lock is held (or was, momentarily) and at least one thread may be blocked on the
/// kernel event, so a release *must* signal it.
const CONTENDED: u32 = 2;

/// How many times to spin before paying for a kernel transition.
///
/// Measured on this host (`advisor/probes/xproc_mutex_probe.c`): with a short
/// critical section and 8 threads per process across 2 processes, a 200-iteration
/// spin resolves 99.8% of contended acquires without touching the kernel
/// (319,666 fast vs 334 blocked out of 320,000). Setting it to 0 pushes 14,114 of
/// the same 320,000 through `WaitForSingleObject` -- still correct, but ~40× more
/// kernel work for no benefit.
const SPIN_LIMIT: u32 = 200;

/// The process-shared half of a cross-process mutex.
///
/// This struct is `repr(C)`, POD, contains no pointers and has no `Drop`, so it is
/// safe to place at a fixed offset inside a section mapped into several processes at
/// *different* base addresses. It is deliberately *not* self-contained: it holds no
/// handle, because a `HANDLE` is a per-process value and would be meaningless to a
/// peer. The kernel event is supplied separately by [`CrossProcessEvent`], which each
/// process opens for itself.
///
/// [`FREE`] (zero) is the correct initial value, so a freshly zeroed page is already
/// a valid array of unlocked mutexes -- which matters, because a pagefile-backed
/// section is zero-filled on first commit and this type is intended to be placed into
/// one without any explicit construction pass.
#[repr(C)]
#[derive(Debug)]
pub struct CrossProcessMutex {
    state: AtomicU32,
}

// A `CrossProcessMutex` is shared across threads *and* processes by construction;
// all of its state is a single atomic.
unsafe impl Send for CrossProcessMutex {}
unsafe impl Sync for CrossProcessMutex {}

impl Default for CrossProcessMutex {
    fn default() -> Self {
        Self::new()
    }
}

impl CrossProcessMutex {
    /// A new, unlocked mutex. `const` so it can also be used as a static initializer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: AtomicU32::new(FREE),
        }
    }

    /// The raw state word, for callers that need to inspect or reset it (e.g. a
    /// process that has just created a fresh shared section).
    #[must_use]
    pub fn raw_state(&self) -> &AtomicU32 {
        &self.state
    }

    /// Attempt to acquire without ever blocking.
    ///
    /// Returns `true` if the lock was acquired.
    pub fn try_lock(&self) -> bool {
        // `Acquire` on success: the critical section's subsequent loads must not be
        // reordered before the acquisition. `Relaxed` on failure: no ordering is
        // implied by a failed attempt, and demanding one would be pure cost.
        self.state
            .compare_exchange(FREE, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// Acquire the lock, blocking on `event` if necessary.
    ///
    /// `event` must be the [`CrossProcessEvent`] associated with *this* mutex -- i.e.
    /// every process contending for this mutex must pass an event opened from the
    /// same name. Passing a mismatched event does not cause memory unsafety, but it
    /// does destroy the wakeup protocol and will hang.
    ///
    /// # Lost-wakeup argument
    ///
    /// The dangerous interleaving for any futex-shaped lock is: a waiter decides to
    /// sleep, and the holder releases in the gap before the waiter is actually
    /// registered, so nobody signals and the waiter sleeps forever.
    ///
    /// That window is closed here by making the waiter *write* [`CONTENDED`]
    /// unconditionally with a single [`AtomicU32::swap`] before it sleeps, and making
    /// the releaser *read* the state with a single [`AtomicU32::swap`] as well. Both
    /// are single atomic read-modify-writes on the same location, so they are totally
    /// ordered with respect to each other. Two cases exhaust the possibilities:
    ///
    /// - The waiter's swap lands first. The word is now `CONTENDED`. The releaser's
    ///   swap therefore *returns* `CONTENDED`, so it signals. No wakeup is lost.
    /// - The releaser's swap lands first. The word is now `FREE`. The waiter's swap
    ///   therefore *returns* `FREE`, which means it has just acquired the lock
    ///   itself, and it returns without ever sleeping.
    ///
    /// There is no third case, because there is no point at which the waiter has
    /// "decided to sleep" but not yet published that fact -- deciding and publishing
    /// are the same atomic operation.
    ///
    /// The cost of this scheme is that a waiter which acquires via the second case
    /// leaves the word marked `CONTENDED` rather than `LOCKED`, so its eventual
    /// release performs one unnecessary `SetEvent`. That is a wasted syscall, never a
    /// correctness problem: a spurious signal simply wakes a waiter that re-checks the
    /// state word and (if it loses the race) sleeps again.
    ///
    /// # Errors
    ///
    /// Returns an error only if `WaitForSingleObject` or `SetEvent` fails for a
    /// reason other than a normal wake -- i.e. a genuine OS-level fault such as the
    /// event handle having been closed. A failure here is not recoverable by
    /// retrying and is deliberately surfaced rather than swallowed, because silently
    /// looping on a broken wait is precisely how a hang becomes undiagnosable.
    pub fn lock(&self, event: &CrossProcessEvent) -> io::Result<CrossProcessMutexGuard<'_>> {
        // Fast path: one atomic, no kernel transition.
        if self.try_lock() {
            return Ok(CrossProcessMutexGuard { mutex: self });
        }
        self.lock_slow(event)?;
        Ok(CrossProcessMutexGuard { mutex: self })
    }

    #[cold]
    fn lock_slow(&self, event: &CrossProcessEvent) -> io::Result<()> {
        // Bounded spin. Cheap, and empirically resolves the overwhelming majority of
        // real contention on the short critical sections this primitive is for.
        for _ in 0..SPIN_LIMIT {
            core::hint::spin_loop();
            if self.try_lock() {
                return Ok(());
            }
        }

        loop {
            // See the lost-wakeup argument on `lock`. `AcqRel` because this single
            // operation is both an acquisition (when it returns `FREE`) and a
            // publication of our intent to sleep (when it does not).
            if self.state.swap(CONTENDED, Ordering::AcqRel) == FREE {
                return Ok(());
            }
            event.wait()?;
        }
    }

    /// Release the lock.
    ///
    /// Normally reached through [`CrossProcessMutexGuard`]'s `Drop`; exposed for the
    /// case where the guard's lifetime is inconvenient (e.g. a lock acquired in one
    /// process and, by design, released in another after a fork).
    ///
    /// # Errors
    ///
    /// Returns an error if `SetEvent` fails.
    ///
    /// # Panics
    ///
    /// Panics if the lock was not held. Unlocking a free mutex is always a bug in the
    /// caller and is never something this primitive should paper over.
    pub fn unlock(&self, event: &CrossProcessEvent) -> io::Result<()> {
        // A single swap both frees the lock and tells us whether anyone is waiting.
        // Reading the state and then storing `FREE` as two separate operations would
        // reintroduce exactly the lost-wakeup window the acquire path is careful to
        // close.
        //
        // `Release` so every store made inside the critical section is visible to the
        // next acquirer before it can observe `FREE`.
        match self.state.swap(FREE, Ordering::Release) {
            CONTENDED => event.signal(),
            LOCKED => Ok(()),
            FREE => panic!("CrossProcessMutex::unlock called on an unlocked mutex"),
            other => panic!("CrossProcessMutex state word corrupted: {other}"),
        }
    }
}

/// RAII guard returned by [`CrossProcessMutex::lock`].
///
/// The guard deliberately does *not* hold a reference to the event, so that a caller
/// which needs to release explicitly (across a fork, say) can do so via
/// [`CrossProcessMutex::unlock`]. Dropping the guard without having unlocked is a
/// bug, and is caught: see this type's `Drop`.
#[derive(Debug)]
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct CrossProcessMutexGuard<'a> {
    mutex: &'a CrossProcessMutex,
}

impl CrossProcessMutexGuard<'_> {
    /// Release the lock explicitly, surfacing any OS error.
    ///
    /// Prefer this over dropping the guard wherever the caller can act on a failure.
    ///
    /// # Errors
    ///
    /// Returns an error if `SetEvent` fails while waking a waiter.
    pub fn unlock(self, event: &CrossProcessEvent) -> io::Result<()> {
        let mutex = self.mutex;
        // Do not let `Drop` run: it would unlock a second time.
        core::mem::forget(self);
        mutex.unlock(event)
    }
}

impl Drop for CrossProcessMutexGuard<'_> {
    fn drop(&mut self) {
        // A guard dropped without `unlock` cannot wake a waiter, because it has no
        // event to signal. Rather than silently leaving a waiter asleep forever --
        // the single hardest failure mode to diagnose in a lock -- free the word and
        // make the mistake loud.
        //
        // This is reachable only on a code path that ignored `#[must_use]` and let
        // the guard fall out of scope, which is a caller bug by construction.
        let prev = self.mutex.state.swap(FREE, Ordering::Release);
        assert_ne!(
            prev, CONTENDED,
            "CrossProcessMutexGuard dropped while a waiter was blocked; \
             use `guard.unlock(&event)` so the waiter can be woken"
        );
    }
}

/// The kernel-object half of a cross-process mutex: a named, auto-reset `Event`.
///
/// Each process opens this for itself by name; no handle ever crosses a process
/// boundary, so there is no `DuplicateHandle` and no process-handle registry to keep
/// alive. `CreateEventW` opens the existing object when one with that name already
/// exists (reporting `ERROR_ALREADY_EXISTS`, which is the *expected* outcome for every
/// participant but the first, not a failure).
///
/// Auto-reset is required: a manual-reset event would leave the event signaled after
/// a wake, so every subsequent waiter would return immediately and spin hot.
#[derive(Debug)]
pub struct CrossProcessEvent {
    handle: HANDLE,
}

// The handle is owned, immutable for the lifetime of the value, and every Win32
// operation performed on it here is itself thread-safe.
unsafe impl Send for CrossProcessEvent {}
unsafe impl Sync for CrossProcessEvent {}

impl CrossProcessEvent {
    /// Open (creating if necessary) the named auto-reset event backing a mutex.
    ///
    /// `name` must be identical in every participating process. Use a `Local\`
    /// prefix to scope the object to the current session, or `Global\` to cross
    /// session boundaries (which requires `SeCreateGlobalPrivilege` and is not needed
    /// for litebox's fork model, where every guest process is a descendant of the
    /// same runner in the same session).
    ///
    /// `SECURITY_ATTRIBUTES` is passed as null deliberately: the default descriptor
    /// grants full access to the creating process's token, and every participating
    /// litebox process is a descendant running under that same token. An inheritable
    /// handle is likewise unnecessary, precisely because the object is found by name
    /// rather than inherited -- which is what makes this design survive
    /// `RtlCloneUserProcess` as well as `CreateProcessW`.
    ///
    /// # Errors
    ///
    /// Returns the OS error if `CreateEventW` fails.
    pub fn open(name: &str) -> io::Result<Self> {
        let wide: Vec<u16> = name.encode_utf16().chain(core::iter::once(0)).collect();

        // SAFETY: `wide` is a valid NUL-terminated UTF-16 buffer that outlives the
        // call. A null `SECURITY_ATTRIBUTES` requests the default descriptor.
        let handle = unsafe {
            CreateEventW(
                core::ptr::null(),
                0, // bManualReset = FALSE -> auto-reset, as required (see type docs)
                0, // bInitialState = FALSE -> not signaled
                wide.as_ptr(),
            )
        };

        if handle.is_null() {
            return Err(io::Error::from_raw_os_error(
                // SAFETY: reading the calling thread's last-error value.
                i32::try_from(unsafe { GetLastError() }).unwrap_or(-1),
            ));
        }

        // NOTE: `ERROR_ALREADY_EXISTS` here is the normal, expected result for every
        // process after the first, and is explicitly *not* treated as an error. The
        // handle returned in that case refers to the existing object, which is exactly
        // what is wanted.

        Ok(Self { handle })
    }

    /// Block until signaled.
    ///
    /// # Errors
    ///
    /// Returns an error if the wait fails for an OS-level reason. A `WAIT_ABANDONED`
    /// result is impossible for an event (it is specific to mutex objects) and is
    /// reported as an error rather than silently accepted.
    pub fn wait(&self) -> io::Result<()> {
        // SAFETY: `self.handle` is a valid event handle owned by this value.
        let rc = unsafe { WaitForSingleObject(self.handle, INFINITE) };
        match rc {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_FAILED => Err(io::Error::from_raw_os_error(
                // SAFETY: reading the calling thread's last-error value.
                i32::try_from(unsafe { GetLastError() }).unwrap_or(-1),
            )),
            other => Err(io::Error::other(format!(
                "WaitForSingleObject on cross-process event returned unexpected {other:#x}"
            ))),
        }
    }

    /// Block until signaled, or until `timeout_ms` elapses.
    ///
    /// Returns `Ok(true)` if signaled, `Ok(false)` if it timed out.
    ///
    /// Provided for callers that need a bounded wait (a watchdog around a lock that
    /// should never be held long, say). The mutex itself waits without a timeout,
    /// because a timeout there could only be handled by looping, which would turn a
    /// genuine lost wakeup into an invisible busy-wait.
    ///
    /// # Errors
    ///
    /// Returns an error if the wait fails for an OS-level reason.
    pub fn wait_timeout(&self, timeout_ms: u32) -> io::Result<bool> {
        // SAFETY: `self.handle` is a valid event handle owned by this value.
        let rc = unsafe { WaitForSingleObject(self.handle, timeout_ms) };
        match rc {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            WAIT_FAILED => {
                // SAFETY: reading the calling thread's last-error value.
                let err = unsafe { GetLastError() };
                if err == ERROR_TIMEOUT {
                    return Ok(false);
                }
                Err(io::Error::from_raw_os_error(
                    i32::try_from(err).unwrap_or(-1),
                ))
            }
            other => Err(io::Error::other(format!(
                "WaitForSingleObject on cross-process event returned unexpected {other:#x}"
            ))),
        }
    }

    /// Wake exactly one waiter (auto-reset semantics).
    ///
    /// Public because the same event is also usable as a standalone cross-process
    /// one-shot barrier, independently of any mutex. Calling it while no waiter is
    /// blocked simply leaves the event signaled, so the *next* waiter returns
    /// immediately -- correct for a barrier, and harmless for the mutex, whose
    /// acquire loop re-checks the state word after every wake.
    ///
    /// # Errors
    ///
    /// Returns an error if `SetEvent` fails.
    pub fn signal(&self) -> io::Result<()> {
        // SAFETY: `self.handle` is a valid event handle owned by this value.
        if unsafe { SetEvent(self.handle) } == 0 {
            return Err(io::Error::from_raw_os_error(
                // SAFETY: reading the calling thread's last-error value.
                i32::try_from(unsafe { GetLastError() }).unwrap_or(-1),
            ));
        }
        Ok(())
    }
}

impl Drop for CrossProcessEvent {
    fn drop(&mut self) {
        // SAFETY: `self.handle` is a valid, owned handle not used after this point.
        // The kernel object itself outlives this close for as long as any other
        // process still holds a handle to it, which is the whole point of naming it.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

// ---------------------------------------------------------------------------
// Integration sketch (design note, deliberately not wired up yet).
//
// `ADVISORY-002` Track B orders this primitive *before* the fixed-base shared kernel
// heap (step 3) and before the `beyond_stdio` gate relaxation (step 4), so nothing
// here is reachable from the runtime yet and nothing above changes existing
// behaviour. When the time comes, the shape is:
//
//   1. `litebox::platform::RawMutex` is already precisely futex semantics
//      (`underlying_atomic()`, `wake_many(n)`, `block(val)`, `block_or_timeout`), and
//      every shim subsystem reaches synchronization only through
//      `RawSyncPrimitivesProvider`, which bottoms out in the single `RawMutex` impl in
//      `lib.rs`. That is one impl to replace, not a diffuse refactor.
//
//   2. The trait *can* be satisfied by this primitive, but not by this type alone,
//      and the reason is worth recording. `RawMutex` requires `const INIT: Self` and
//      hands out a `&AtomicU32` that callers wake by *address*. A cross-process impl
//      cannot carry its `CrossProcessEvent` in `INIT`, because opening a kernel object
//      is not a `const` operation. The natural resolution is a process-wide side
//      table keyed by the state word's offset *within the shared section* -- never by
//      its address, which differs per process (measured; see this module's docs) --
//      mapping each offset to a lazily opened `CrossProcessEvent`. `INIT` then stays
//      trivially `const`, and `block`/`wake_many` resolve the event on first use.
//
//   3. That side table wants the shared section's base to be known, which is exactly
//      what Track B step 3 (fixed-base shared section behind `SafeZoneAllocator`)
//      provides. Hence the ordering: this primitive is the building block, and the
//      trait impl lands with the allocator work rather than ahead of it.
//
// Deliberately left as a note rather than speculative code, per Track B's own
// step-by-step ordering.
// ---------------------------------------------------------------------------
