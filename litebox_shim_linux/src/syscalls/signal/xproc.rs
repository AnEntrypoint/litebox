//! Cross-process process registry and signal delivery.
//!
//! Under `LITEBOX_PROCESS_FORK=1` a guest `fork()` child can be a separate host process, so the
//! parent's `Arc<Process>` cannot reach it and `kill()` had nowhere to deliver. Every guest
//! process is therefore registered in [`SharedProcessTable`], a pointer-free field of
//! `GlobalState` (which lives in the cross-process shared kernel arena, so every host process in
//! the fork family sees the same slots). A slot maps a guest pid to the host process that runs
//! it, its process group, and a bitmask of signals posted to it from other host processes.
//!
//! Delivery: a sender ORs the signal's bit into the target slot's `pending` and wakes the target
//! host process's signal listener (a platform thread blocked on a named event). The listener
//! drains every slot owned by its host process into the matching `Process::shared_pending` and
//! interrupts that process's threads, exactly like an in-process `kill()`. Each task also drains
//! its own slot whenever it checks for pending signals, so a wake that raced the listener's
//! startup is never lost. `SIGKILL` to a process that owns its whole host process terminates that
//! host process with the encoded `WIFSIGNALED(SIGKILL)` exit code instead, so it works even when
//! the target never reaches a signal check.
//!
//! Stop/continue (`SIGSTOP`/`SIGTSTP`/`SIGCONT`) cross the boundary like any other signal; the
//! target then applies whatever it does for them in-process, and this module adds no
//! job-control stop semantics of its own.

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use alloc::sync::{Arc, Weak};
use litebox_common_linux::errno::Errno;
use litebox_common_linux::signal::Signal;

use crate::syscalls::process::{ExitStatus, Process, encode_cross_process_exit_status};
use crate::{GlobalStateHandle, ShimFS, ShimPlatform, Task};

pub(crate) const SHARED_PROCESS_CAPACITY: usize = 512;
pub(crate) const NO_SLOT: u32 = u32::MAX;

const SLOT_FREE: i32 = 0;
const SLOT_CLAIMING: i32 = -1;

struct ProcessSlot {
    pid: AtomicI32,
    host_pid: AtomicU32,
    pgid: AtomicI32,
    pending: AtomicU64,
    owns_host: AtomicBool,
}

impl ProcessSlot {
    const fn new() -> Self {
        Self {
            pid: AtomicI32::new(SLOT_FREE),
            host_pid: AtomicU32::new(0),
            pgid: AtomicI32::new(0),
            pending: AtomicU64::new(0),
            owns_host: AtomicBool::new(false),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SlotView {
    pub(crate) index: u32,
    pub(crate) pid: i32,
    pub(crate) host_pid: u32,
    pub(crate) pgid: i32,
    pub(crate) owns_host: bool,
}

pub(crate) struct SharedProcessTable {
    slots: [ProcessSlot; SHARED_PROCESS_CAPACITY],
}

impl SharedProcessTable {
    pub(crate) fn new() -> Self {
        Self {
            slots: [const { ProcessSlot::new() }; SHARED_PROCESS_CAPACITY],
        }
    }

    fn slot(&self, index: u32, pid: i32) -> Option<&ProcessSlot> {
        let slot = self.slots.get(index as usize)?;
        (pid > 0 && slot.pid.load(Ordering::Acquire) == pid).then_some(slot)
    }

    pub(crate) fn find(&self, pid: i32) -> Option<u32> {
        if pid <= 0 {
            return None;
        }
        self.slots
            .iter()
            .position(|s| s.pid.load(Ordering::Acquire) == pid)
            .map(|i| i as u32)
    }

    fn view(&self, index: u32) -> Option<SlotView> {
        let slot = self.slots.get(index as usize)?;
        let pid = slot.pid.load(Ordering::Acquire);
        (pid > 0).then(|| SlotView {
            index,
            pid,
            host_pid: slot.host_pid.load(Ordering::Acquire),
            pgid: slot.pgid.load(Ordering::Acquire),
            owns_host: slot.owns_host.load(Ordering::Acquire),
        })
    }

    pub(crate) fn lookup(&self, pid: i32) -> Option<SlotView> {
        self.find(pid).and_then(|i| self.view(i))
    }

    pub(crate) fn members(&self) -> impl Iterator<Item = SlotView> + '_ {
        (0..SHARED_PROCESS_CAPACITY as u32).filter_map(|i| self.view(i))
    }

    /// Returns the slot for `pid`, claiming one if it has none. An existing slot keeps its
    /// `pgid` and pending signals (a parent may already have moved the child into another group
    /// or signalled it before the child registered itself) and only has its host updated. When
    /// every slot is taken, slots whose host process has died are reclaimed.
    pub(crate) fn register(
        &self,
        pid: i32,
        host_pid: u32,
        pgid: i32,
        owns_host: bool,
        host_alive: impl Fn(u32) -> bool,
    ) -> Option<u32> {
        if pid <= 0 {
            return None;
        }
        if let Some(index) = self.find(pid) {
            let slot = &self.slots[index as usize];
            slot.host_pid.store(host_pid, Ordering::Release);
            slot.owns_host.store(owns_host, Ordering::Release);
            return Some(index);
        }
        for attempt in 0..2 {
            for (i, slot) in self.slots.iter().enumerate() {
                if slot
                    .pid
                    .compare_exchange(
                        SLOT_FREE,
                        SLOT_CLAIMING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    slot.host_pid.store(host_pid, Ordering::Relaxed);
                    slot.pgid.store(pgid, Ordering::Relaxed);
                    slot.pending.store(0, Ordering::Relaxed);
                    slot.owns_host.store(owns_host, Ordering::Relaxed);
                    slot.pid.store(pid, Ordering::Release);
                    return Some(i as u32);
                }
            }
            if attempt == 0 {
                self.reclaim_dead_hosts(&host_alive);
            }
        }
        None
    }

    fn reclaim_dead_hosts(&self, host_alive: &impl Fn(u32) -> bool) {
        for slot in &self.slots {
            let pid = slot.pid.load(Ordering::Acquire);
            if pid > 0 && !host_alive(slot.host_pid.load(Ordering::Acquire)) {
                let _ =
                    slot.pid
                        .compare_exchange(pid, SLOT_FREE, Ordering::AcqRel, Ordering::Acquire);
            }
        }
    }

    pub(crate) fn set_host(&self, index: u32, pid: i32, host_pid: u32, owns_host: bool) {
        if let Some(slot) = self.slot(index, pid) {
            slot.host_pid.store(host_pid, Ordering::Release);
            slot.owns_host.store(owns_host, Ordering::Release);
        }
    }

    pub(crate) fn pgid(&self, index: u32, pid: i32) -> Option<i32> {
        self.slot(index, pid)
            .map(|s| s.pgid.load(Ordering::Acquire))
    }

    pub(crate) fn set_pgid(&self, index: u32, pid: i32, pgid: i32) {
        if let Some(slot) = self.slot(index, pid) {
            slot.pgid.store(pgid, Ordering::Release);
        }
    }

    pub(crate) fn unregister(&self, pid: i32) {
        if let Some(index) = self.find(pid) {
            let slot = &self.slots[index as usize];
            slot.pending.store(0, Ordering::Relaxed);
            let _ = slot
                .pid
                .compare_exchange(pid, SLOT_FREE, Ordering::AcqRel, Ordering::Acquire);
        }
    }

    fn post(&self, index: u32, pid: i32, signal: Signal) -> bool {
        let Some(slot) = self.slot(index, pid) else {
            return false;
        };
        slot.pending.fetch_or(signal_bit(signal), Ordering::AcqRel);
        true
    }

    fn has_pending(&self, index: u32) -> bool {
        self.slots
            .get(index as usize)
            .is_some_and(|s| s.pending.load(Ordering::Relaxed) != 0)
    }

    fn take_pending(&self, index: u32, pid: i32) -> u64 {
        self.slot(index, pid)
            .map_or(0, |s| s.pending.swap(0, Ordering::AcqRel))
    }
}

/// The fork family's table, published once per host process by [`publish_process_table`] so a
/// caller with no `GlobalStateHandle` -- `/proc`, which is mounted by `default_fs` before
/// `GlobalState` exists -- can still ask whether a pid is live. A raw pointer rather than a
/// reference because the hook that consumes it is a plain `fn` and can carry no state; it is valid
/// in every host process of the family because the table lives inside `GlobalState`, which is
/// allocated in the cross-process shared arena at one fixed address in all of them.
static PUBLISHED_PROCESS_TABLE: AtomicUsize = AtomicUsize::new(0);

/// Hands `/proc` (via `litebox::fs::procfs::set_pid_known_fn`) the only cross-process answer to
/// "does guest `pid` exist": [`pid_is_known`]. Called from `LinuxShimBuilder::build`, so every
/// host process of the fork family -- including each cross-process fork child -- publishes its own
/// view of the one shared table.
pub(crate) fn publish_process_table(table: &SharedProcessTable) {
    PUBLISHED_PROCESS_TABLE.store(table as *const SharedProcessTable as usize, Ordering::Release);
}

/// Whether some host process in this fork family is running guest `pid`. False before
/// [`publish_process_table`] runs, which leaves `/proc/<pid>` at `ENOENT` rather than inventing a
/// process.
pub(crate) fn pid_is_known(pid: i32) -> bool {
    let raw = PUBLISHED_PROCESS_TABLE.load(Ordering::Acquire);
    if raw == 0 {
        return false;
    }
    // SAFETY: `PUBLISHED_PROCESS_TABLE` only ever holds the address stored by
    // `publish_process_table`, which is the `SharedProcessTable` inside this fork family's
    // `GlobalState`; that allocation outlives every caller here, and the shared arena is mapped at
    // the same address in each host process, so the reference is valid in whichever one runs this.
    let table = unsafe { &*(raw as *const SharedProcessTable) };
    table.find(pid).is_some()
}

const GRACEFUL_SIGKILL_EXIT_LIMIT_MS: u32 = 1000;

fn signal_bit(signal: Signal) -> u64 {
    1u64 << (signal.as_i32() - 1)
}

fn signals_in(bits: u64) -> impl Iterator<Item = Signal> {
    (1..=64i32)
        .filter(move |n| bits & (1u64 << (n - 1)) != 0)
        .filter_map(|n| Signal::try_from(n).ok())
}

fn deliver_bits<Platform: ShimPlatform>(process: &Process<Platform>, bits: u64) {
    if bits == 0 {
        return;
    }
    {
        let mut pending = process.shared_pending.lock();
        for signal in signals_in(bits) {
            pending.push(&process.limits, signal, super::siginfo_kill(signal));
        }
    }
    process.interrupt_all_threads();
}

/// Local (this host process only) map from guest pid to the `Process` that runs here, so the
/// signal listener can turn a drained slot back into a `Process` to deliver into.
pub(crate) type LocalProcessMap<Platform> =
    litebox::sync::Mutex<Platform, alloc::collections::BTreeMap<i32, Weak<Process<Platform>>>>;

/// Drains every slot owned by this host process into its `Process`. Runs on the platform's
/// signal-listener thread.
fn drain_host<Platform: ShimPlatform, FS: ShimFS>(global: &GlobalStateHandle<Platform, FS>) {
    let host = global.platform.current_host_pid();
    let table = &global.process_table;
    for view in table.members() {
        if view.host_pid != host {
            continue;
        }
        let process = global
            .xproc_local
            .lock()
            .get(&view.pid)
            .and_then(Weak::upgrade);
        if let Some(process) = process {
            if table.has_pending(view.index) {
                let bits = table.take_pending(view.index, view.pid);
                if view.owns_host && bits & signal_bit(Signal::SIGKILL) != 0 {
                    global.platform.exit_host_process_quiesced(
                        encode_cross_process_exit_status(ExitStatus::Signal(Signal::SIGKILL)),
                    );
                }
                deliver_bits(&process, bits);
            }
            // The listener also fires for cross-process data events (`wake_signal_listener` after
            // a write to a shared connection): every thread blocked in a poll/read re-checks
            // readiness now instead of at its next bounded repoll tick.
            litebox::event::polling::bump_external_wake_epoch();
            process.wake_waiting_threads();
        }
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    fn xproc_host(&self) -> Option<u32> {
        let host = self.global.platform.current_host_pid();
        (host != 0).then_some(host)
    }

    /// Registers the calling task's own process (bootstrap or cross-process fork child) and
    /// starts this host process's signal listener. `owns_host` marks a process that is the whole
    /// of its host process, which lets `SIGKILL` terminate the host process directly.
    pub(crate) fn xproc_register_self(&self, owns_host: bool) {
        let Some(host) = self.xproc_host() else {
            return;
        };
        let process = self.process();
        self.xproc_register_local(self.pid.get(), &process, host, owns_host);
        let global = self.global.clone();
        self.global
            .platform
            .start_signal_wake_listener(alloc::boxed::Box::new(move || drain_host(&global)));
    }

    /// Registers a process that runs in this host process under guest pid `pid`.
    pub(crate) fn xproc_register_local(
        &self,
        pid: i32,
        process: &Arc<Process<Platform>>,
        host: u32,
        owns_host: bool,
    ) {
        let platform = self.global.platform;
        let Some(index) = self.global.process_table.register(
            pid,
            host,
            process.pgid.load(Ordering::Relaxed),
            owns_host,
            |h| platform.is_process_alive(h),
        ) else {
            return;
        };
        if let Some(pgid) = self.global.process_table.pgid(index, pid) {
            process.pgid.store(pgid, Ordering::Relaxed);
        }
        process.xproc_slot.store(index, Ordering::Release);
        let mut local = self.global.xproc_local.lock();
        local.retain(|_, p| p.strong_count() > 0);
        local.insert(pid, Arc::downgrade(process));
    }

    /// Registers a forked child that is still being set up: its host is provisionally this host
    /// process, so signals posted before it is running wait in its slot.
    pub(crate) fn xproc_preregister_child(&self, child_pid: i32, pgid: i32) {
        let Some(host) = self.xproc_host() else {
            return;
        };
        let platform = self.global.platform;
        let _ = self
            .global
            .process_table
            .register(child_pid, host, pgid, false, |h| {
                platform.is_process_alive(h)
            });
    }

    /// Points a pre-registered child's slot at the host process `spawn_cross_process_fork_child`
    /// just created for it.
    pub(crate) fn xproc_child_spawned(
        &self,
        child_pid: i32,
        handle: litebox::platform::CrossProcessChildHandle,
    ) {
        let table = &self.global.process_table;
        if let (Some(index), Some(host)) = (
            table.find(child_pid),
            self.global.platform.cross_process_child_host_pid(handle),
        ) {
            table.set_host(index, child_pid, host, true);
        }
    }

    pub(crate) fn xproc_unregister(&self, pid: i32) {
        if self.xproc_host().is_none() {
            return;
        }
        self.global.process_table.unregister(pid);
        self.global.xproc_local.lock().remove(&pid);
    }

    /// Moves the calling task's own slot's pending bits into its process's `shared_pending`.
    pub(crate) fn xproc_drain_own(&self) {
        let process = self.process();
        let index = process.xproc_slot.load(Ordering::Acquire);
        if index == NO_SLOT || !self.global.process_table.has_pending(index) {
            return;
        }
        let bits = self
            .global
            .process_table
            .take_pending(index, self.pid.get());
        if bits == 0 {
            return;
        }
        let mut pending = process.shared_pending.lock();
        for signal in signals_in(bits) {
            pending.push(&process.limits, signal, super::siginfo_kill(signal));
        }
    }

    /// The process group of `pid` as the registry records it.
    pub(crate) fn xproc_pgid_of(&self, pid: i32) -> Option<i32> {
        self.xproc_host()?;
        self.global.process_table.lookup(pid).map(|v| v.pgid)
    }

    /// Records `pgid` for `pid` in the registry and, when that process runs here, in its local
    /// `Process` too. Returns whether `pid` is registered.
    pub(crate) fn xproc_set_pgid(&self, pid: i32, pgid: i32) -> bool {
        if self.xproc_host().is_none() {
            return false;
        }
        let table = &self.global.process_table;
        let Some(index) = table.find(pid) else {
            return false;
        };
        table.set_pgid(index, pid, pgid);
        if let Some(process) = self
            .global
            .xproc_local
            .lock()
            .get(&pid)
            .and_then(Weak::upgrade)
        {
            process.pgid.store(pgid, Ordering::Relaxed);
        }
        true
    }

    /// Delivers `signal` (`None` = existence probe) to registered process `pid`. `ESRCH` if the
    /// registry is disabled or `pid` is not registered.
    pub(crate) fn xproc_send(&self, pid: i32, signal: Option<Signal>) -> Result<(), Errno> {
        let my_host = self.xproc_host().ok_or(Errno::ESRCH)?;
        let view = self.global.process_table.lookup(pid).ok_or(Errno::ESRCH)?;
        if !self.global.xproc_alive_or_release(view, my_host) {
            return Err(Errno::ESRCH);
        }
        self.global.xproc_send_to(view, my_host, signal);
        Ok(())
    }

    /// Delivers `signal` to every registered process other than the caller that `select`
    /// accepts. Returns the pids it reached (empty when the registry is disabled).
    pub(crate) fn xproc_send_many(
        &self,
        signal: Option<Signal>,
        select: impl Fn(&SlotView) -> bool,
    ) -> alloc::vec::Vec<i32> {
        let me = self.pid.get();
        self.global
            .xproc_send_many(signal, |v| v.pid != me && select(v))
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> GlobalStateHandle<Platform, FS> {
    /// Sends `signal` to every registered member of process group `pgid` (the pty line
    /// discipline's `ISIG` path, which has no sending `Task`). Returns whether any member was
    /// reached.
    pub(crate) fn xproc_signal_group(&self, pgid: i32, signal: Signal) -> bool {
        !self
            .xproc_send_many(Some(signal), |v| v.pgid == pgid)
            .is_empty()
    }

    fn xproc_send_many(
        &self,
        signal: Option<Signal>,
        select: impl Fn(&SlotView) -> bool,
    ) -> alloc::vec::Vec<i32> {
        let my_host = self.platform.current_host_pid();
        if my_host == 0 {
            return alloc::vec::Vec::new();
        }
        let targets: alloc::vec::Vec<SlotView> = self
            .process_table
            .members()
            .filter(|v| select(v))
            .filter(|v| self.xproc_alive_or_release(*v, my_host))
            .collect();
        for view in &targets {
            self.xproc_send_to(*view, my_host, signal);
        }
        targets.iter().map(|v| v.pid).collect()
    }

    /// Wakes every OTHER host process's blocked waiters. The network stack (`Network`) is shared
    /// but readiness notification is per process (`Pollee` observers are local), so a socket state
    /// change noticed by whichever process ran the poll -- an incoming connection, data, a close --
    /// would otherwise leave a waiter in a different process asleep until its own next tick.
    pub(crate) fn xproc_poke_other_hosts(&self) {
        let me = self.platform.current_host_pid();
        if me == 0 {
            return;
        }
        let mut poked: alloc::vec::Vec<u32> = alloc::vec::Vec::new();
        for view in self.process_table.members() {
            if view.host_pid != me && !poked.contains(&view.host_pid) {
                poked.push(view.host_pid);
                self.platform.wake_signal_listener(view.host_pid);
            }
        }
    }

    /// A process that was its whole host process is gone once that host process is. Such a slot
    /// is released here, since an orphan's slot is never released by a reap.
    fn xproc_alive_or_release(&self, view: SlotView, my_host: u32) -> bool {
        if view.owns_host
            && view.host_pid != my_host
            && !self.platform.is_process_alive(view.host_pid)
        {
            self.process_table.unregister(view.pid);
            return false;
        }
        true
    }

    fn xproc_send_to(&self, view: SlotView, my_host: u32, signal: Option<Signal>) {
        let Some(signal) = signal else { return };
        if view.host_pid == my_host {
            let local = self
                .xproc_local
                .lock()
                .get(&view.pid)
                .and_then(Weak::upgrade);
            if let Some(process) = local {
                deliver_bits(&process, signal_bit(signal));
                return;
            }
            self.process_table.post(view.index, view.pid, signal);
            return;
        }
        if signal == Signal::SIGKILL
            && view.owns_host
            && self.process_table.post(view.index, view.pid, signal)
            && self.platform.wake_signal_listener(view.host_pid)
            && self.platform.wait_for_host_process_exit(
                view.host_pid,
                GRACEFUL_SIGKILL_EXIT_LIMIT_MS,
            )
        {
            return;
        }
        if signal == Signal::SIGKILL && view.owns_host {
            litebox_util_log::warn!(
                target_pid:% = view.pid, target_host:% = view.host_pid, sender_host:% = my_host;
                "xproc SIGKILL: target did not exit gracefully, terminating its host process"
            );
        }
        if signal == Signal::SIGKILL
            && view.owns_host
            && self.platform.terminate_host_process(
                view.host_pid,
                encode_cross_process_exit_status(ExitStatus::Signal(Signal::SIGKILL)),
            )
        {
            return;
        }
        if self.process_table.post(view.index, view.pid, signal) {
            self.platform.wake_signal_listener(view.host_pid);
        }
    }
}
