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
            return Err(FutexError::ImmediatelyWokenBecauseValueMismatch);
        }
        // Only return when woken--don't reevaluate the futex word. This
        // ensures that the rate control mechanisms provided by the futex
        // interface are effective.
        cx.wait_until(|| entry.get().done.load(Ordering::Acquire))
            .map_err(FutexError::WaitError)
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
