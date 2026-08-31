// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! `timerfd_create(2)`/`timerfd_settime(2)`/`timerfd_gettime(2)`: a file descriptor that becomes
//! readable once an armed deadline passes, delivering the number of expirations that have
//! occurred since the last `read()` (matching real Linux `timerfd(2)` semantics).
//!
//! Deliberately readiness-only, no push-based wakeup: [`TimerfdFile::check_io_events`] compares
//! the current time (via a stored `&'static Platform`, the same handle every other subsystem in
//! this crate reaches through `GlobalState`) against the stored deadline whenever it is polled --
//! the same "recheck on whatever schedule the caller already polls at" shape `IOPollable` uses
//! everywhere else in this shim. This is sufficient for every real-world timerfd consumer this
//! shim's current goals care about (glib/libwayland event loops, matching this crate's
//! `signalfd.rs`'s identical narrowing rationale) because such an event loop always integrates a
//! timerfd via `epoll`, and always computes its own `epoll_wait` timeout from the earliest
//! pending timer deadline as an optimization -- so the fd is polled again, and observed ready, at
//! (or very close to) the exact moment it expires. A blocking direct `read()` with no other fd to
//! wake the epoll loop is not a real-world usage pattern for a timerfd and is not specially
//! optimized here.

use core::sync::atomic::AtomicU32;

use litebox::{
    event::{Events, IOPollable, observer::Observer, polling::Pollee},
    fd::{FdEnabledSubsystem, FdEnabledSubsystemEntry},
    fs::OFlags,
    platform::TimeProvider,
    sync::{Mutex, RawSyncPrimitivesProvider},
};
use litebox_common_linux::{TfdFlags, errno::Errno};

use crate::ShimPlatform;

pub(crate) struct TimerfdSubsystem<Platform: ShimPlatform>(core::marker::PhantomData<Platform>);
impl<Platform: ShimPlatform> FdEnabledSubsystem for TimerfdSubsystem<Platform> {
    type Entry = TimerfdFile<Platform>;
}
impl<Platform: ShimPlatform> FdEnabledSubsystemEntry for TimerfdFile<Platform> {}

struct TimerState<Instant> {
    /// `None` means disarmed (never armed, or disarmed via a zero `it_value`).
    deadline: Option<Instant>,
    /// Zero means "single-shot, do not repeat" (real Linux `it_interval == 0` semantics).
    interval: core::time::Duration,
    /// Expirations accumulated since the last successful `read()`, matching real Linux's `u64`
    /// expiration counter. A periodic timer that missed multiple intervals (host was busy) is
    /// honestly reported as however many whole intervals have actually elapsed, not clamped to 1
    /// -- computed fresh from `deadline`/`interval`/now each time it's observed, never
    /// incremented by a separate background driver (there is none; see module doc comment).
    accrued: u64,
}

impl<Instant> Default for TimerState<Instant> {
    fn default() -> Self {
        Self {
            deadline: None,
            interval: core::time::Duration::ZERO,
            accrued: 0,
        }
    }
}

impl<Instant: litebox::platform::Instant> TimerState<Instant> {
    /// Recomputes `accrued` and, for a periodic timer, advances `deadline` past `now` -- called
    /// on every observation point (`check_io_events`, `read`, `gettime`) so no background driver
    /// is needed.
    fn resync(&mut self, now: Instant) {
        let Some(deadline) = self.deadline else {
            return;
        };
        let overdue_opt = now.checked_duration_since(&deadline);
        let remaining_opt = deadline.checked_duration_since(&now);
        litebox_util_log::debug!(
            overdue:? = overdue_opt,
            remaining_if_future:? = remaining_opt;
            "DIAG TimerState::resync"
        );
        let Some(overdue) = overdue_opt else {
            return; // deadline is still in the future
        };
        if self.interval.is_zero() {
            // Single-shot: exactly one expiration, then disarmed until re-armed.
            self.accrued = self.accrued.saturating_add(1);
            self.deadline = None;
            return;
        }
        // Periodic: count every whole interval that has elapsed (including ones the caller never
        // observed in time -- real Linux's own `timerfd_read` semantics), then advance the
        // deadline to the next one still in the future.
        let interval_nanos = self.interval.as_nanos().max(1);
        let elapsed_nanos = overdue.as_nanos() + interval_nanos;
        let missed = elapsed_nanos / interval_nanos;
        self.accrued = self
            .accrued
            .saturating_add(u64::try_from(missed).unwrap_or(u64::MAX));
        let advance = self
            .interval
            .saturating_mul(u32::try_from(missed).unwrap_or(u32::MAX));
        self.deadline = deadline.checked_add(advance);
    }

    fn remaining(&self, now: Instant) -> core::time::Duration {
        self.deadline
            .as_ref()
            .and_then(|d| d.checked_duration_since(&now))
            .unwrap_or(core::time::Duration::ZERO)
    }
}

pub(crate) struct TimerfdFile<Platform: RawSyncPrimitivesProvider + TimeProvider + 'static> {
    platform: &'static Platform,
    state: Mutex<Platform, TimerState<Platform::Instant>>,
    /// File status flags (see [`OFlags::STATUS_FLAGS_MASK`])
    status: AtomicU32,
    pollee: Pollee<Platform>,
    /// Whether this fd was created with `CLOCK_REALTIME` (`true`) rather than `CLOCK_MONOTONIC`
    /// or one of its close cousins (`false`) -- set once at `timerfd_create(2)` time, consulted
    /// by `sys_timerfd_settime`'s `TFD_TIMER_ABSTIME` handling to convert the guest's absolute
    /// deadline into this platform's monotonic `Instant` domain against the RIGHT epoch. Getting
    /// this wrong (previously: always assuming realtime, regardless of what the guest actually
    /// requested) meant a `CLOCK_MONOTONIC`-based absolute deadline -- a small "seconds since some
    /// monotonic reference point" value -- compared as smaller than wall-clock "now" (a ~1.7-billion-
    /// second Unix timestamp), which unconditionally took the "already past" branch and armed the
    /// timer to fire immediately. Confirmed live: this is exactly what starved weston's own
    /// internal event-loop timerfd of ever legitimately expiring on its own schedule -- it fired
    /// immediately on every arm, and because weston's dispatch callback for it never actually
    /// needed to run yet, nothing called `read()`, leaving the fd stuck permanently
    /// `Events::IN`-ready and its owning thread spinning in `epoll_wait` forever.
    is_realtime: bool,
}

impl<Platform: RawSyncPrimitivesProvider + TimeProvider + 'static> TimerfdFile<Platform> {
    pub(crate) fn new(platform: &'static Platform, flags: TfdFlags, is_realtime: bool) -> Self {
        let mut status = OFlags::RDONLY;
        status.set(OFlags::NONBLOCK, flags.contains(TfdFlags::NONBLOCK));

        Self {
            platform,
            state: Mutex::new(TimerState::default()),
            status: AtomicU32::new(status.bits()),
            pollee: Pollee::new(),
            is_realtime,
        }
    }

    /// Whether this fd was created with `CLOCK_REALTIME` -- see [`Self::is_realtime`]'s doc
    /// comment. Consulted by `sys_timerfd_settime`'s `TFD_TIMER_ABSTIME` handling.
    pub(crate) fn is_realtime(&self) -> bool {
        self.is_realtime
    }

    /// Arms/disarms the timer per `timerfd_settime(2)` semantics. `value`/`interval` are already
    /// resolved to this platform's monotonic clock domain by the caller (see `sys_timerfd_settime`
    /// for `TFD_TIMER_ABSTIME` handling). Returns the previous `(it_interval, it_value_remaining)`.
    pub(crate) fn set_time(
        &self,
        deadline: Option<Platform::Instant>,
        interval: core::time::Duration,
    ) -> (core::time::Duration, core::time::Duration) {
        let now = self.platform.now();
        let mut state = self.state.lock();
        state.resync(now);
        let prev = (state.interval, state.remaining(now));

        state.deadline = deadline;
        state.interval = interval;
        state.accrued = 0;
        // Re-check readiness against the NEW deadline before deciding whether to notify: an
        // unconditional `notify_observers` here (the previous behavior) marks any registered
        // `EpollEntry` observer as immediately "ready" via its `Observer::on_events` ->
        // `ReadySet::push` -> `is_ready = true`, REGARDLESS of whether the new deadline is
        // actually due yet. Confirmed live via a full XFCE repro trace
        // (`.wfgy/xfce-build/epolldiag1_clean.log`): weston re-arms its own repaint timerfd with
        // a legitimate near-future deadline (~9-14ms out), this spuriously marks the epoll
        // interest `is_ready`, and `EpollFile::has_unready_stdin_or_armed_timerfd_interest`
        // (which short-circuits `is_ready` entries as "already ready, no bounded repoll needed")
        // then lets weston's own `epoll_pwait(timeout=None)` call commit to an UNBOUNDED wait --
        // permanently, since nothing else was pending to wake it, and the real (not-yet-actually-
        // ready) timerfd is never re-observed until some unrelated fd traffic happens to wake the
        // same epoll instance first. Only notifying when the new deadline is ALREADY due (i.e.
        // `resync` against `now` immediately finds it overdue) preserves the one case that
        // legitimately needs an immediate wakeup, without falsely marking a genuinely-future
        // deadline "ready".
        state.resync(now);
        let already_due = state.accrued > 0;
        drop(state);
        if already_due {
            self.pollee.notify_observers(Events::IN);
        }
        prev
    }

    pub(crate) fn get_time(&self) -> (core::time::Duration, core::time::Duration) {
        let now = self.platform.now();
        let mut state = self.state.lock();
        state.resync(now);
        (state.interval, state.remaining(now))
    }

    fn try_read(&self) -> Result<u64, Errno> {
        let now = self.platform.now();
        let mut state = self.state.lock();
        state.resync(now);
        if state.accrued == 0 {
            return Err(Errno::EAGAIN);
        }
        let n = state.accrued;
        state.accrued = 0;
        Ok(n)
    }

    /// Note: deliberately not integrated with `WaitContext`/`Pollee::wait`'s blocking-retry loop
    /// -- see module doc comment: this fd has no background driver to wake a blocked reader, so a
    /// blocking `read()` here would hang forever rather than waking at the deadline. The one
    /// real-world usage pattern (poll/epoll-integrated, `O_NONBLOCK`) never takes this path
    /// blocking, matching `signalfd.rs`'s identical, explicitly-documented narrowing.
    pub(crate) fn read(&self) -> Result<u64, Errno> {
        if !self.get_status().contains(OFlags::NONBLOCK) {
            return Err(Errno::EOPNOTSUPP);
        }
        self.try_read()
    }

    super::common_functions_for_file_status!();
}

impl<Platform: RawSyncPrimitivesProvider + TimeProvider + 'static> IOPollable for TimerfdFile<Platform> {
    fn check_io_events(&self) -> Events {
        let now = self.platform.now();
        let mut state = self.state.lock();
        state.resync(now);
        if state.accrued > 0 {
            Events::IN
        } else {
            Events::empty()
        }
    }

    fn register_observer(&self, observer: alloc::sync::Weak<dyn Observer<Events>>, mask: Events) {
        self.pollee.register_observer(observer, mask);
    }
}

#[cfg(test)]
mod tests {
    use litebox::{
        event::{Events, IOPollable as _},
        platform::{Instant as _, TimeProvider as _},
    };
    use litebox_common_linux::{TfdFlags, errno::Errno};

    extern crate std;

    fn platform() -> &'static crate::syscalls::tests::TestPlatform {
        crate::syscalls::tests::test_platform(None)
    }

    #[test]
    fn disarmed_timerfd_is_never_ready_and_read_returns_eagain() {
        let _task = crate::syscalls::tests::init_platform(None);
        let tfd = super::TimerfdFile::new(platform(), TfdFlags::NONBLOCK, false);
        assert_eq!(tfd.check_io_events(), Events::empty());
        assert_eq!(tfd.read(), Err(Errno::EAGAIN));
    }

    #[test]
    fn single_shot_timer_becomes_ready_and_reports_one_expiration() {
        let _task = crate::syscalls::tests::init_platform(None);
        let tfd = super::TimerfdFile::new(platform(), TfdFlags::NONBLOCK, false);
        let now = platform().now();
        let deadline = now.checked_add(core::time::Duration::from_millis(20));
        tfd.set_time(deadline, core::time::Duration::ZERO);

        // Not yet due.
        assert_eq!(tfd.check_io_events(), Events::empty());
        assert_eq!(tfd.read(), Err(Errno::EAGAIN));

        std::thread::sleep(core::time::Duration::from_millis(60));

        assert_eq!(tfd.check_io_events(), Events::IN);
        assert_eq!(tfd.read(), Ok(1));
        // Single-shot: does not re-arm, and the expiration count is consumed.
        assert_eq!(tfd.check_io_events(), Events::empty());
        assert_eq!(tfd.read(), Err(Errno::EAGAIN));
    }

    #[test]
    fn periodic_timer_accrues_multiple_missed_expirations() {
        let _task = crate::syscalls::tests::init_platform(None);
        let tfd = super::TimerfdFile::new(platform(), TfdFlags::NONBLOCK, false);
        let now = platform().now();
        let interval = core::time::Duration::from_millis(10);
        let deadline = now.checked_add(interval);
        tfd.set_time(deadline, interval);

        // Sleep long enough for several intervals to have elapsed before the first observation
        // -- real Linux's own timerfd semantics report every whole interval that passed, not
        // just one, matching this shim's `TimerState::resync` doc comment.
        std::thread::sleep(core::time::Duration::from_millis(55));

        let n = tfd.read().unwrap();
        assert!(n >= 4, "expected at least 4 missed intervals, got {n}");
        // The interval keeps re-arming: still readable (or about to be) on future ticks.
        let (interval_out, _remaining) = tfd.get_time();
        assert_eq!(interval_out, interval);
    }

    #[test]
    fn set_time_with_zero_value_disarms() {
        let _task = crate::syscalls::tests::init_platform(None);
        let tfd = super::TimerfdFile::new(platform(), TfdFlags::NONBLOCK, false);
        let now = platform().now();
        tfd.set_time(
            now.checked_add(core::time::Duration::from_millis(5)),
            core::time::Duration::ZERO,
        );
        tfd.set_time(None, core::time::Duration::ZERO);
        std::thread::sleep(core::time::Duration::from_millis(30));
        assert_eq!(tfd.check_io_events(), Events::empty());
        assert_eq!(tfd.read(), Err(Errno::EAGAIN));
    }

    #[test]
    fn blocking_read_without_nonblock_is_a_documented_narrow_gap() {
        let _task = crate::syscalls::tests::init_platform(None);
        let tfd = super::TimerfdFile::new(platform(), TfdFlags::empty(), false);
        assert_eq!(tfd.read(), Err(Errno::EOPNOTSUPP));
    }
}
