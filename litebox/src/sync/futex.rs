// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! A Linux-y `futex`-like abstraction. Fast user-space mutexes.

// Implementation note: other submodules of `crate::sync` should NOT depend on
// this module directly, because this module itself depends on some of the other
// modules (specifically, this module depends on `LoanList`, which depends on
// `Mutex`). A refactoring could clean this up and prevent this dependency, but
// at the moment, it has been decided that this ordering of dependency is more
// fruitful.

use core::hash::BuildHasher as _;
use core::num::NonZeroU32;
use core::pin::pin;
use core::sync::atomic::{AtomicBool, Ordering};

use super::RawSyncPrimitivesProvider;
use crate::event::wait::{WaitContext, WaitError, Waker};
use crate::platform::RawPointerProvider;
use crate::platform::{RawConstPointer as _, TimeProvider};
use crate::utilities::loan_list::{LoanList, LoanListEntry};
use crate::utils::TruncateExt as _;
use thiserror::Error;

/// A manager of all available futexes.
///
/// Note: currently, this only supports "private" futexes, since it assumes only a single process.
/// In the future, this may be expanded to support multi-process futexes.
pub struct FutexManager<Platform: RawSyncPrimitivesProvider> {
    /// Chaining hash table to map from futex address to waiter lists.
    table: alloc::boxed::Box<[LoanList<Platform, FutexEntry<Platform>>; HASH_TABLE_ENTRIES]>,
    hash_builder: hashbrown::DefaultHashBuilder,
}

/// The number of buckets in the hash table.
///
/// FUTURE: consider making this scale with some property of the platform, such
/// as number of CPUs.
const HASH_TABLE_ENTRIES: usize = 256;

/// How often a parked futex wait re-reads its word, to catch a lost wakeup (see `wait`).
const FUTEX_WORD_CHECK_SLICE: core::time::Duration = core::time::Duration::from_secs(2);

/// Diagnostic: waits currently parked in `FutexManager::wait`, process-wide. A wait claims a slot
/// on entry and releases it on return, so a dump lists the whole wait-for set.
const DIAG_WAIT_SLOTS: usize = 256;

static DIAG_WAIT_ADDR: [core::sync::atomic::AtomicUsize; DIAG_WAIT_SLOTS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; DIAG_WAIT_SLOTS];

/// Packs the waiter's expected value (high 32 bits) and bitset (low 32 bits).
static DIAG_WAIT_INFO: [core::sync::atomic::AtomicU64; DIAG_WAIT_SLOTS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; DIAG_WAIT_SLOTS];

/// Diagnostic: `FUTEX_WAKE`s per address bucket, process-wide. A parked waiter records its bucket's
/// count when it parks, so a dump shows whether any wake reached that address since.
static DIAG_WAKES: [core::sync::atomic::AtomicU64; 256] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 256];

/// The bucket count each parked waiter saw when it parked (indexed like `DIAG_WAIT_ADDR`).
static DIAG_PARK_WAKES: [core::sync::atomic::AtomicU64; DIAG_WAIT_SLOTS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; DIAG_WAIT_SLOTS];

fn diag_bucket(addr: usize) -> usize {
    (addr >> 2) % 256
}

fn diag_claim(addr: usize, expected: u32, bitset: u32) -> Option<usize> {
    for (slot, (addr_slot, info_slot)) in DIAG_WAIT_ADDR.iter().zip(&DIAG_WAIT_INFO).enumerate() {
        if addr_slot
            .compare_exchange(0, addr, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            let packed = (u64::from(expected) << 32) | u64::from(bitset);
            info_slot.store(packed, Ordering::Release);
            DIAG_PARK_WAKES[slot]
                .store(DIAG_WAKES[diag_bucket(addr)].load(Ordering::Relaxed), Ordering::Relaxed);
            return Some(slot);
        }
    }
    None
}

fn diag_release(slot: Option<usize>) {
    if let Some(slot) = slot {
        DIAG_WAIT_ADDR[slot].store(0, Ordering::Release);
    }
}

fn diag_dump_parked(reporter_addr: usize) {
    for (index, (addr_slot, info_slot)) in DIAG_WAIT_ADDR.iter().zip(&DIAG_WAIT_INFO).enumerate() {
        let addr = addr_slot.load(Ordering::Acquire);
        if addr == 0 {
            continue;
        }
        let info = info_slot.load(Ordering::Acquire);
        let wakes_since_park = DIAG_WAKES[diag_bucket(addr)]
            .load(Ordering::Relaxed)
            .wrapping_sub(DIAG_PARK_WAKES[index].load(Ordering::Relaxed));
        litebox_util_log::warn!(
            addr:% = addr,
            expected:% = info >> 32,
            bitset:% = info & 0xffff_ffff,
            wakes_since_park:% = wakes_since_park,
            reporter:% = addr == reporter_addr;
            "diag-futex-parked: a FUTEX_WAIT is parked"
        );
    }
}

struct FutexEntry<Platform: RawSyncPrimitivesProvider> {
    addr: usize,
    waker: Waker<Platform>,
    bitset: u32,
    done: AtomicBool,
}

const ALL_BITS: NonZeroU32 = NonZeroU32::new(u32::MAX).unwrap();

impl<Platform: RawSyncPrimitivesProvider + RawPointerProvider + TimeProvider>
    FutexManager<Platform>
{
    /// A new futex manager.
    // TODO(jayb): Integrate this into the `litebox` object itself, to prevent the possibility of
    // double-creation.
    #[expect(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            table: alloc::boxed::Box::new(core::array::from_fn(|_| LoanList::new())),
            hash_builder: hashbrown::DefaultHashBuilder::default(),
        }
    }

    /// Returns the hash table bucket for the given futex address.
    fn bucket(&self, addr: usize) -> &LoanList<Platform, FutexEntry<Platform>> {
        let hash: usize = self.hash_builder.hash_one(addr).trunc();
        &self.table[hash % HASH_TABLE_ENTRIES]
    }

    /// Performs a futex wait.
    ///
    /// This function tests once if the futex word matches the expected value,
    /// returning immediately with
    /// [`FutexError::ImmediatelyWokenBecauseValueMismatch`] if it does not.
    /// Otherwise, it waits until woken by a corresponding until
    /// [`FutexManager::wake`] is called targeting the same futex word or until
    /// the wait times out or is interrupted.
    ///
    /// If `bitset` is `Some`, then the waiter is only woken if the wake call's
    /// `bitset` has a non-zero intersection with the waiter's mask. Specifying
    /// `None` is equivalent to setting all bits in the mask.
    pub fn wait(
        &self,
        cx: &WaitContext<'_, Platform>,
        futex_addr: Platform::RawMutPointer<u32>,
        expected_value: u32,
        bitset: Option<NonZeroU32>,
    ) -> Result<(), FutexError> {
        let bitset = bitset.unwrap_or(ALL_BITS).get();
        let addr = futex_addr.as_usize();
        if !addr.is_multiple_of(align_of::<u32>()) {
            return Err(FutexError::NotAligned);
        }

        let bucket = self.bucket(addr);
        let mut entry = pin!(LoanListEntry::new(FutexEntry {
            addr,
            waker: cx.waker().clone(),
            bitset,
            done: AtomicBool::new(false),
        },));

        // Insert into the bucket's list. It will be removed when woken or the
        // entry goes out of scope.
        entry.as_mut().insert(bucket);

        // Check the value once. Do this only after inserting into the list so
        // that we don't miss a wakeup.
        let value = futex_addr.read_at_offset(0).ok_or(FutexError::Fault)?;
        if value != expected_value {
            // We were already visible to wakers, so a concurrent `wake` may have extracted this
            // entry and spent one unit of its `num_to_wake` on a thread that is not going to sleep.
            // Real Linux checks the value before the waiter is visible, so this can never happen
            // there; here the wake must be passed on, or a genuinely sleeping waiter is never
            // woken (a lost wakeup: the futex word is already changed, so no later wake follows).
            if entry.get().done.load(Ordering::Acquire) {
                let _ = self.wake(
                    futex_addr,
                    NonZeroU32::MIN,
                    Some(NonZeroU32::new(bitset).unwrap_or(ALL_BITS)),
                );
            }
            return Err(FutexError::ImmediatelyWokenBecauseValueMismatch);
        }
        // Only return when woken--don't reevaluate the futex word. This
        // ensures that the rate control mechanisms provided by the futex
        // interface are effective.
        // Diagnostic: the readiness check runs once per re-park, so the count names a wait that
        // keeps re-parking. Report the guest word once it passes 1024 re-parks, with `done` and
        // the value a FUTEX_WAKE would have to change.
        let polls = core::cell::Cell::new(0u32);
        let diag_slot = diag_claim(addr, expected_value, bitset);
        let mut ready = || {
            let n = polls.get().wrapping_add(1);
            polls.set(n);
            let done = entry.get().done.load(Ordering::Acquire);
            if n == 1024 && !done {
                let word = futex_addr.read_at_offset(0).unwrap_or(u32::MAX);
                litebox_util_log::warn!(
                    addr:% = addr,
                    expected:% = expected_value,
                    word:% = word,
                    bitset:% = bitset;
                    "diag-futex-spin: guest FUTEX_WAIT re-polled 1024 times without done"
                );
            }
            // At 65536 re-parks the wait is past any healthy one: dump every parked waiter, so a
            // stall shows the wait-for set rather than a single entry.
            if n == 65_536 && !done {
                diag_dump_parked(addr);
            }
            done
        };
        // Wait in slices. A slice that times out (not the caller's own deadline) re-reads the
        // word: if it has changed while `done` is still false, the wake this waiter needs was
        // never delivered (a lost wakeup), and the log says so.
        let mut slices: u32 = 0;
        let waited = loop {
            match cx.with_timeout(FUTEX_WORD_CHECK_SLICE).wait_until(&mut ready) {
                Err(WaitError::TimedOut)
                    if !(cx.deadline().is_some() && cx.remaining_timeout().is_none()) =>
                {
                    // A wait parked for a minute with no wake is the stall shape: dump every parked
                    // waiter once, with the wakes each saw since it parked.
                    slices += 1;
                    if slices == 30 {
                        diag_dump_parked(addr);
                    }
                    let word = futex_addr.read_at_offset(0).unwrap_or(u32::MAX);
                    if word != expected_value && !entry.get().done.load(Ordering::Acquire) {
                        litebox_util_log::warn!(
                            addr:% = addr,
                            expected:% = expected_value,
                            word:% = word,
                            bitset:% = bitset;
                            "diag-futex-lost-wake: the word changed and no wake reached this waiter"
                        );
                    }
                }
                result => break result,
            }
        };
        diag_release(diag_slot);
        match waited {
            Ok(()) => Ok(()),
            // A wake that raced our timeout or interrupt has already consumed this entry: report
            // success (as Linux does when the waker wins), otherwise the wake is silently lost.
            Err(_) if entry.get().done.load(Ordering::Acquire) => Ok(()),
            Err(e) => Err(FutexError::WaitError(e)),
        }
    }

    /// Wakes waiters on the given futex word.
    ///
    /// This operation wakes at most `num_to_wake` of the waiters that are
    /// waiting on the futex word. Most commonly, `num_to_wake` is specified as
    /// either 1 (wake up a single waiter) or max value (to wake up all
    /// waiters). No guarantee is provided about which waiters are awoken.
    ///
    /// If `bitset` is `Some`, then it contains a mask that specifies which
    /// waiters to wake up. Specifically, any waiters that have a non-zero
    /// intersection between their masks and the provided `bitset` can be woken,
    /// (subject to the `num_to_wake` limit). If `bitset` is `None`, then all
    /// waiters are eligible to be woken.
    ///
    /// Returns the number of waiters that were woken up.
    pub fn wake(
        &self,
        futex_addr: Platform::RawMutPointer<u32>,
        num_to_wake_up: NonZeroU32,
        bitset: Option<NonZeroU32>,
    ) -> Result<u32, FutexError> {
        let addr = futex_addr.as_usize();
        if !addr.is_multiple_of(align_of::<u32>()) {
            return Err(FutexError::NotAligned);
        }
        DIAG_WAKES[diag_bucket(addr)].fetch_add(1, Ordering::Relaxed);
        let bitset = bitset.unwrap_or(ALL_BITS).get();
        let mut woken = 0;
        let bucket = self.bucket(addr);
        // Extract matching entries from the bucket until we've woken enough.
        let entries = bucket.extract_if(|entry| {
            if entry.addr != addr || entry.bitset & bitset == 0 {
                return core::ops::ControlFlow::Continue(false);
            }
            woken += 1;
            if woken >= num_to_wake_up.get() {
                core::ops::ControlFlow::Break(true)
            } else {
                core::ops::ControlFlow::Continue(true)
            }
        });
        // A wake that finds nothing is the interesting case: it distinguishes "a waiter was
        // registered on this address but did not match" from "no waiter was there at all when
        // the wake fired". The first is a queue/bitset bug; the second is a lost-wakeup race
        // where the wake ran before the waiter parked. Reporting every address currently queued
        // in this bucket separates them: an address close to the target means the waiter is
        // present under a different key, while an empty or unrelated bucket means the waiter
        // had not registered yet.
        if woken == 0 {
            litebox_util_log::debug!(
                addr:% = addr,
                requested:% = num_to_wake_up.get();
                "futex: WAKE matched nothing"
            );
        }

        // Wake the waiters outside the `extract_if` closure to minimize the list's lock hold
        // time.
        for entry in entries {
            // `Release` here is required to actually pair with `wait`'s `Acquire` load below --
            // a `Relaxed` store paired with an `Acquire` load establishes no happens-before edge
            // at all (the "acquire" side would have nothing to synchronize with), so the waiter
            // waking up from `Waker::wake()`'s OS-level notification would not be guaranteed to
            // observe this write. In practice this was likely masked on x86's strong memory model
            // and by `Waker::wake()`'s own internal `Release` fetch_update ordering the store
            // before it in this thread's program order, but relying on that is fragile and not
            // guaranteed by the abstract memory model this code is written against.
            entry.done.store(true, Ordering::Release);
            entry.waker.wake();
        }
        Ok(woken)
    }

    /// Implements `FUTEX_REQUEUE`/`FUTEX_CMP_REQUEUE`.
    ///
    /// Real Linux wakes up to `wake_count` waiters on `futex_addr` directly, then *moves* up to
    /// `requeue_count` of the remaining waiters onto `requeue_addr`'s wait queue without waking
    /// them, so a later wake on `requeue_addr` can reach them (this is how musl's
    /// `pthread_cond_broadcast`/`pthread_cond_signal` avoid a thundering herd when moving a
    /// condition variable's waiters onto its associated mutex).
    ///
    /// This implementation cannot perform that move: each waiter's `FutexEntry` is pinned on
    /// the waiting thread's own stack (owned by that thread's call to [`Self::wait`]), not owned
    /// by `FutexManager`, so there is no entry for this call to transplant into a different
    /// bucket. Instead, every waiter that real Linux would have requeued is woken directly here
    /// instead of being moved. This is always a safe, spec-compliant approximation --
    /// `FUTEX_WAIT` callers are already required to re-check their real wait condition after any
    /// wakeup (spurious wakeups are explicitly permitted by futex(2)), which is exactly what
    /// musl's condvar code does with the mutex it reacquires after waking. It only gives up the
    /// thundering-herd-avoidance optimization, not correctness.
    ///
    /// Waking eagerly instead of silently dropping the requeue matters: before this was
    /// implemented at all, `FUTEX_REQUEUE`/`FUTEX_CMP_REQUEUE` were rejected with `EINVAL` at the
    /// syscall-decoding layer, which is how musl's `pthread_cond_broadcast` was observed to
    /// permanently strand condition-variable waiters (e.g. libuv threadpool workers in Node.js)
    /// -- the real, reproduced root cause of an intermittent process-exit hang.
    ///
    /// If `expected_value` is `Some`, the futex word at `futex_addr` must still equal it or this
    /// returns [`FutexError::ImmediatelyWokenBecauseValueMismatch`] without waking anyone
    /// (`FUTEX_CMP_REQUEUE`'s documented race-closing check).
    pub fn requeue(
        &self,
        futex_addr: Platform::RawMutPointer<u32>,
        wake_count: u32,
        requeue_count: u32,
        _requeue_addr: Platform::RawMutPointer<u32>,
        expected_value: Option<u32>,
    ) -> Result<u32, FutexError> {
        let addr = futex_addr.as_usize();
        if !addr.is_multiple_of(align_of::<u32>()) {
            return Err(FutexError::NotAligned);
        }
        if let Some(expected_value) = expected_value {
            let value = futex_addr.read_at_offset(0).ok_or(FutexError::Fault)?;
            if value != expected_value {
                return Err(FutexError::ImmediatelyWokenBecauseValueMismatch);
            }
        }

        let total_to_wake = wake_count.saturating_add(requeue_count);
        let Some(total_to_wake) = NonZeroU32::new(total_to_wake) else {
            return Ok(0);
        };
        let mut woken = 0;
        let bucket = self.bucket(addr);
        let entries = bucket.extract_if(|entry| {
            if entry.addr != addr {
                return core::ops::ControlFlow::Continue(false);
            }
            woken += 1;
            if woken >= total_to_wake.get() {
                core::ops::ControlFlow::Break(true)
            } else {
                core::ops::ControlFlow::Continue(true)
            }
        });
        for entry in entries {
            // See `wake`'s doc comment on why `Release` is required here.
            entry.done.store(true, Ordering::Release);
            entry.waker.wake();
        }
        Ok(woken)
    }
}

/// Potential errors that can be returned by [`FutexManager`]'s operations.
#[derive(Debug, Error)]
pub enum FutexError {
    #[error("address not correctly aligned to 4-bytes")]
    NotAligned,
    #[error("immediately woken: value did not match expected")]
    ImmediatelyWokenBecauseValueMismatch,
    #[error("wait error")]
    WaitError(WaitError),
    #[error("fault reading futex word")]
    Fault,
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::LiteBox;
    use crate::event::wait::WaitState;
    use crate::platform::mock::MockPlatform;
    use alloc::sync::Arc;
    use core::num::NonZeroU32;
    use core::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Barrier;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_futex_wait_wake_single_thread() {
        let platform = MockPlatform::new();
        let _litebox = LiteBox::new(platform);
        let futex_manager = Arc::new(FutexManager::new());

        let futex_word = Arc::new(AtomicU32::new(0));
        let barrier = Arc::new(Barrier::new(2));

        let futex_manager_clone = Arc::clone(&futex_manager);
        let futex_word_clone = Arc::clone(&futex_word);
        let barrier_clone = Arc::clone(&barrier);

        // Spawn waiter thread
        let waiter = thread::spawn(move || {
            let futex_addr =
                <MockPlatform as crate::platform::RawPointerProvider>::RawMutPointer::from_usize(
                    futex_word_clone.as_ptr() as usize,
                );

            barrier_clone.wait(); // Sync with main thread

            // Wait for value 0
            futex_manager_clone.wait(&WaitState::new(platform).context(), futex_addr, 0, None)
        });

        barrier.wait(); // Wait for waiter to be ready
        thread::sleep(Duration::from_millis(10)); // Give waiter time to block

        // Change the value and wake
        futex_word.store(1, Ordering::SeqCst);
        let futex_addr =
            <MockPlatform as crate::platform::RawPointerProvider>::RawMutPointer::from_usize(
                futex_word.as_ptr() as usize,
            );
        let woken = futex_manager
            .wake(futex_addr, NonZeroU32::new(1).unwrap(), None)
            .unwrap();

        // Wait for waiter thread to complete
        let result = waiter.join().unwrap();
        assert!(result.is_ok());
        assert_eq!(woken, 1);
    }

    #[test]
    fn test_futex_wait_wake_single_thread_with_timeout() {
        let platform = MockPlatform::new();
        let _litebox = LiteBox::new(platform);
        let futex_manager = Arc::new(FutexManager::new());

        let futex_word = Arc::new(AtomicU32::new(0));
        let barrier = Arc::new(Barrier::new(2));

        let futex_manager_clone = Arc::clone(&futex_manager);
        let futex_word_clone = Arc::clone(&futex_word);
        let barrier_clone = Arc::clone(&barrier);

        // Spawn waiter thread with timeout
        let waiter_thread = thread::spawn(move || {
            let futex_addr =
                <MockPlatform as crate::platform::RawPointerProvider>::RawMutPointer::from_usize(
                    futex_word_clone.as_ptr() as usize,
                );

            barrier_clone.wait(); // Sync with main thread

            // Wait for value 0 with some timeout
            futex_manager_clone.wait(
                &WaitState::new(platform)
                    .context()
                    .with_timeout(Duration::from_millis(300)),
                futex_addr,
                0,
                None,
            )
        });

        barrier.wait(); // Wait for waiter to be ready
        thread::sleep(Duration::from_millis(30)); // Give waiter time to block

        // Change the value and wake
        futex_word.store(1, Ordering::SeqCst);
        let futex_addr =
            <MockPlatform as crate::platform::RawPointerProvider>::RawMutPointer::from_usize(
                futex_word.as_ptr() as usize,
            );
        let woken = futex_manager
            .wake(futex_addr, NonZeroU32::new(1).unwrap(), None)
            .unwrap();

        // Wait for waiter thread to complete
        let result = waiter_thread.join().unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(woken, 1);
    }

    #[test]
    fn test_futex_multiple_waiters_with_timeout() {
        let platform = MockPlatform::new();
        let _litebox = LiteBox::new(platform);
        let futex_manager = Arc::new(FutexManager::new());

        let futex_word = Arc::new(AtomicU32::new(0));
        let barrier = Arc::new(Barrier::new(4)); // 3 waiters + 1 waker

        let mut waiters = std::vec::Vec::new();

        // Spawn 3 waiter threads with timeout
        for _ in 0..3 {
            let futex_manager_clone = Arc::clone(&futex_manager);
            let futex_word_clone = Arc::clone(&futex_word);
            let barrier_clone = Arc::clone(&barrier);

            let waiter = thread::spawn(move || {
                let futex_addr = <MockPlatform as crate::platform::RawPointerProvider>::RawMutPointer::from_usize(
                    futex_word_clone.as_ptr() as usize
                );

                barrier_clone.wait(); // Sync with other threads

                // Wait for value 0 with some timeout
                futex_manager_clone.wait(
                    &WaitState::new(platform)
                        .context()
                        .with_timeout(Duration::from_millis(300)),
                    futex_addr,
                    0,
                    None,
                )
            });
            waiters.push(waiter);
        }

        barrier.wait(); // Wait for all waiters to be ready
        thread::sleep(Duration::from_millis(10)); // Give waiters time to block

        // Change the value and wake all
        futex_word.store(1, Ordering::SeqCst);
        let futex_addr =
            <MockPlatform as crate::platform::RawPointerProvider>::RawMutPointer::from_usize(
                futex_word.as_ptr() as usize,
            );
        let woken = futex_manager
            .wake(futex_addr, NonZeroU32::new(u32::MAX).unwrap(), None)
            .unwrap();

        // Wait for all waiter threads to complete
        for waiter in waiters {
            let result = waiter.join().unwrap();
            match result {
                Ok(())
                | Err(
                    FutexError::WaitError(_) | FutexError::ImmediatelyWokenBecauseValueMismatch,
                ) => {}
                Err(FutexError::NotAligned | FutexError::Fault) => {
                    unreachable!()
                }
            }
        }

        assert!((1..=3).contains(&woken));
    }

    /// Regression test for the real, reproduced `node -e "console.log(1)"` intermittent hang:
    /// `FUTEX_REQUEUE`/`FUTEX_CMP_REQUEUE` (used by musl's `pthread_cond_broadcast` to move
    /// condition-variable waiters onto their mutex without a thundering herd) must actually wake
    /// every requeued waiter -- not silently drop them -- since `FutexManager::requeue` cannot
    /// truly transplant a waiter's pinned, stack-owned entry into a different bucket and instead
    /// wakes everyone real Linux would have requeued. Before `requeue` existed, the futex
    /// syscall's `op` decoding rejected `FUTEX_REQUEUE`/`FUTEX_CMP_REQUEUE` outright with
    /// `EINVAL`, so this exact call was never reached and these waiters were never woken at all.
    #[test]
    fn test_futex_requeue_wakes_all_requeued_waiters() {
        let platform = MockPlatform::new();
        let _litebox = LiteBox::new(platform);
        let futex_manager = Arc::new(FutexManager::new());

        let futex_word = Arc::new(AtomicU32::new(0));
        let other_word = Arc::new(AtomicU32::new(0));
        let barrier = Arc::new(Barrier::new(4)); // 3 waiters + 1 requeuer

        let mut waiters = std::vec::Vec::new();
        for _ in 0..3 {
            let futex_manager_clone = Arc::clone(&futex_manager);
            let futex_word_clone = Arc::clone(&futex_word);
            let barrier_clone = Arc::clone(&barrier);
            let waiter = thread::spawn(move || {
                let futex_addr =
                    <MockPlatform as crate::platform::RawPointerProvider>::RawMutPointer::from_usize(
                        futex_word_clone.as_ptr() as usize,
                    );
                barrier_clone.wait();
                futex_manager_clone.wait(
                    &WaitState::new(platform)
                        .context()
                        .with_timeout(Duration::from_secs(5)),
                    futex_addr,
                    0,
                    None,
                )
            });
            waiters.push(waiter);
        }

        barrier.wait();
        thread::sleep(Duration::from_millis(10));

        let futex_addr =
            <MockPlatform as crate::platform::RawPointerProvider>::RawMutPointer::from_usize(
                futex_word.as_ptr() as usize,
            );
        let other_addr =
            <MockPlatform as crate::platform::RawPointerProvider>::RawMutPointer::from_usize(
                other_word.as_ptr() as usize,
            );
        // Mirrors musl's pthread_cond_broadcast: wake 1 directly, requeue the rest (here: woken
        // directly too, since this implementation cannot transplant pinned entries).
        let woken = futex_manager
            .requeue(futex_addr, 1, u32::MAX, other_addr, None)
            .unwrap();
        assert_eq!(woken, 3, "requeue should account for every waiter");

        for waiter in waiters {
            let result = waiter.join().unwrap();
            assert!(
                result.is_ok(),
                "every waiter should have been woken by requeue, not left blocked: {result:?}"
            );
        }
    }

    #[test]
    fn test_futex_cmp_requeue_rejects_stale_value() {
        let platform = MockPlatform::new();
        let _litebox = LiteBox::new(platform);
        let futex_manager = FutexManager::<MockPlatform>::new();

        let futex_word = AtomicU32::new(5);
        let other_word = AtomicU32::new(0);
        let futex_addr =
            <MockPlatform as crate::platform::RawPointerProvider>::RawMutPointer::from_usize(
                futex_word.as_ptr() as usize,
            );
        let other_addr =
            <MockPlatform as crate::platform::RawPointerProvider>::RawMutPointer::from_usize(
                other_word.as_ptr() as usize,
            );

        let result = futex_manager.requeue(futex_addr, 1, 1, other_addr, Some(999));
        assert!(matches!(
            result,
            Err(FutexError::ImmediatelyWokenBecauseValueMismatch)
        ));
    }
}
