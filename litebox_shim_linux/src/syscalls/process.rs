// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Process/thread related syscalls.

use crate::{ShimFS, ShimPlatform, Task, UserPtr, UserPtrMut};
use alloc::boxed::Box;
use alloc::collections::btree_map::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::Cell;
use core::mem::offset_of;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use litebox::event::wait::WaitError;
use litebox::mm::linux::VmFlags;
use litebox::platform::TimerHandle;
use litebox::platform::{ArchSpecificRegister, RawMutex as _};
use litebox::platform::{Instant as _, SystemTime as _, TimeProvider};
use litebox::sync::Mutex;
use litebox::utils::TruncateExt as _;
use litebox_common_linux::{
    ArchPrctlArg, CloneFlags, FutexArgs, IntervalTimer, ItimerVal, PrctlArg, TimeParam,
    errno::Errno,
};

/// Process-management-related state on [`Task`].
pub(crate) struct ThreadState<Platform: ShimPlatform> {
    init_state: Cell<ThreadInitState>,
    process: Arc<Process<Platform>>,
    /// Thread state that can be accessed from a remote thread.
    remote: Arc<ThreadRemote<Platform>>,
    attached_tid: Cell<Option<i32>>,
    /// When a thread whose `clear_child_tid` is not `None` terminates, and it shares memory with other threads,
    /// the kernel writes 0 to the address specified by `clear_child_tid` and then executes:
    ///
    /// futex(clear_child_tid, FUTEX_WAKE, 1, NULL, NULL, 0);
    ///
    /// This operation wakes a single thread waiting on the specified memory location via futex.
    /// Any errors from the futex wake operation are ignored.
    clear_child_tid: Cell<Option<UserPtrMut<i32>>>,
    /// The purpose of the robust futex list is to ensure that if a thread accidentally fails to unlock a futex before
    /// terminating or calling execve(2), another thread that is waiting on that futex is notified that the former owner
    /// of the futex has died. This notification consists of two pieces: the FUTEX_OWNER_DIED bit is set in the futex word,
    /// and the kernel performs a futex(2) FUTEX_WAKE operation on one of the threads waiting on the futex.
    robust_list: Cell<Option<UserPtr<litebox_common_linux::RobustListHead>>>,
    /// `(sched_policy, sched_priority)` as last set via `sched_setscheduler`/`sched_setparam`,
    /// defaulting to `(SCHED_OTHER, 0)`. We don't implement real OS-level scheduling-policy
    /// semantics -- this is accept-and-remember state (matching the `TCGETS`/`TCSETS` termios
    /// pattern) purely so `sched_getscheduler`/`sched_getparam` reflect whatever was last set.
    sched_policy_priority: Cell<(i32, i32)>,
}

// TODO: remove once we figure out how to handle Send/Sync for raw pointers.
unsafe impl<Platform: ShimPlatform> Send for ThreadState<Platform> {}

impl<Platform: ShimPlatform> ThreadState<Platform> {
    pub fn new_process(
        pid: i32,
        pm: litebox::mm::PageManager<Platform, { litebox::mm::linux::PAGE_SIZE }>,
        vforked: bool,
        parent: Option<Weak<Process<Platform>>>,
        shared_pending: Arc<Mutex<Platform, super::signal::PendingSignals>>,
    ) -> Self {
        let remote = Arc::new(ThreadRemote::new());
        Self {
            init_state: Cell::new(ThreadInitState::None),
            process: Arc::new(Process::new(
                pid,
                remote.clone(),
                pm,
                vforked,
                parent,
                shared_pending,
            )),
            remote,
            attached_tid: Cell::new(Some(pid)),
            clear_child_tid: Cell::new(None),
            robust_list: Cell::new(None),
            sched_policy_priority: Cell::new((SCHED_OTHER, 0)),
        }
    }

    pub(crate) fn new_thread(&self, tid: i32) -> Option<Self> {
        let remote = self.process.attach_thread(tid)?;
        Some(Self {
            init_state: Cell::new(ThreadInitState::None),
            process: self.process.clone(),
            remote,
            attached_tid: Cell::new(Some(tid)),
            clear_child_tid: Cell::new(None),
            robust_list: Cell::new(None),
            sched_policy_priority: Cell::new((SCHED_OTHER, 0)),
        })
    }

    /// Detaches this thread from its process and immediately wakes any `wait4`/`wait_for_exit`
    /// waiters, returning `true` if this was the process's last thread (i.e. the process has now
    /// fully exited). Returns `false` if this thread was already detached (double-detach is
    /// possible via `ThreadState`'s own `Drop` running after `Task::prepare_for_exit` already
    /// called `detach_from_process_deferred` explicitly) or was never attached.
    ///
    /// Used only by the `Drop` safety-net path below, where no further teardown needs to happen
    /// before waiters are allowed to observe the exit -- the normal exit path goes through
    /// `detach_from_process_deferred` instead, see that function's doc comment.
    fn detach_from_process(&self) -> bool {
        if let Some(tid) = self.attached_tid.take() {
            let (notify, process_exited) = self.process.detach_thread(tid);
            if notify {
                self.process.notify_detached();
            }
            process_exited
        } else {
            false
        }
    }

    /// Like [`Self::detach_from_process`], but defers waking `wait4`/`wait_for_exit` waiters:
    /// returns `(notify, process_exited)` and leaves the caller responsible for calling
    /// [`Process::notify_detached`] itself once any process-exit teardown that
    /// must be externally observable first (fd release, etc.) has completed. See
    /// `Process::detach_thread`'s doc comment for why this ordering matters.
    fn detach_from_process_deferred(&self) -> (bool, bool) {
        if let Some(tid) = self.attached_tid.take() {
            self.process.detach_thread(tid)
        } else {
            (false, false)
        }
    }

    /// Returns the cell that holds this thread's [`litebox::event::wait::ThreadHandle`], used by
    /// [`Task::handle_init_request`] (the real per-guest-thread startup path) and, in tests only,
    /// by `Task::set_thread_handle_for_test` to publish a `ThreadHandle` for a thread spawned via
    /// `spawn_clone_for_test`, which does not go through the real guest-thread-startup path.
    #[cfg(test)]
    pub(crate) fn remote_handle_cell(
        &self,
    ) -> &once_cell::race::OnceBox<litebox::event::wait::ThreadHandle<Platform>> {
        &self.remote.handle
    }
}

impl<Platform: ShimPlatform> Drop for ThreadState<Platform> {
    fn drop(&mut self) {
        // Reparenting (if this is the process's last thread) is handled explicitly by
        // `Task::prepare_for_exit`, which calls `detach_from_process` itself before this `Drop`
        // runs (attached_tid is already `None` by the time we get here in the normal exit path,
        // making this a no-op) -- see that function's doc comment. This `Drop` impl exists purely
        // as a safety net for any path that drops `ThreadState` without going through
        // `Task::prepare_for_exit` first.
        let _ = self.detach_from_process();
    }
}

/// Thread state that can be accessed from a remote thread.
struct ThreadRemote<Platform: ShimPlatform> {
    /// Always set under the process `inner` lock, but can be read without
    /// locking.
    is_exiting: AtomicBool,
    /// Handle to interrupt waits on this thread.
    handle: once_cell::race::OnceBox<litebox::event::wait::ThreadHandle<Platform>>,
}

impl<Platform: ShimPlatform> ThreadRemote<Platform> {
    fn new() -> Self {
        Self {
            is_exiting: AtomicBool::new(false),
            handle: once_cell::race::OnceBox::new(),
        }
    }

    fn interrupt(&self) {
        if let Some(handle) = self.handle.get() {
            handle.interrupt();
        }
    }
}

/// A `(pid, Process)` pair for one of a [`Process`]'s children (see [`Process::children`]).
type ChildEntry<Platform> = (i32, Arc<Process<Platform>>);

/// A Linux process, which may have multiple threads.
pub(crate) struct Process<Platform: ShimPlatform> {
    /// Number of threads in this process. Always updated under the `inner`
    /// mutex lock.
    nr_threads: <Platform as litebox::platform::RawMutexProvider>::RawMutex,
    inner: Mutex<Platform, ProcessInner<Platform>>,
    /// Resource limits for this process.
    pub(crate) limits: ResourceLimits,
    /// Process-wide alarm timer.
    pub(crate) alarm_timer: Mutex<Platform, Alarm<Platform>>,
    /// This process's virtual address space. Shared by every thread in this process
    /// (`CloneFlags::VM`); a forked child process gets its own independent
    /// [`litebox::mm::PageManager`] (see [`litebox::mm::PageManager::duplicate`]) rather than
    /// referencing this one.
    pub(crate) pm: litebox::mm::PageManager<Platform, { litebox::mm::linux::PAGE_SIZE }>,
    /// This process's parent, set once at creation (`do_clone`'s process-clone branch) and never
    /// changed afterward -- a `Weak` reference since the parent may exit (and be fully dropped,
    /// once reaped) before this process does.
    ///
    /// Used purely for orphan reparenting on this process's own exit (see
    /// `Task::prepare_for_exit`'s doc comment): real Linux reparents an orphaned process to its
    /// nearest living ancestor (traditionally PID 1, or a `PR_SET_CHILD_SUBREAPER`), which this
    /// approximates by walking `parent` chains upward from the exiting process until a still-live
    /// (`Weak::upgrade`-able) one is found, falling back to the shim's bootstrap process if the
    /// whole chain is already gone. `None` only for the bootstrap process itself, which has no
    /// shim-visible parent.
    parent: Option<Weak<Process<Platform>>>,
    /// Child processes created via real `fork()`/`vfork()` (i.e. process-style `clone()`,
    /// distinct from thread-style clone which attaches into THIS `Process` rather than
    /// creating a new one). Consumed by `wait4`/`waitpid`. A `(pid, Process)` pair is removed
    /// once successfully waited-for (Linux does not let you wait for the same child twice).
    children: Mutex<Platform, alloc::vec::Vec<ChildEntry<Platform>>>,
    /// Cross-process `fork()` children (pass 141, `LITEBOX_PROCESS_FORK=1`, `beyond_stdio==0`
    /// scope only) -- a genuinely separate Windows OS process, spawned instead of the normal
    /// same-process `spawn_thread` a thread-based `fork()` uses. Such a child cannot share this
    /// process's `Arc<Process>`/`Arc<GlobalState>` (Rust `Arc`s do not cross OS process
    /// boundaries -- see PASS 140 of `scratchpad/jqrepro/FINDINGS.txt`), so it is tracked
    /// separately here by pid, keyed to an opaque
    /// [`litebox::platform::CrossProcessChildHandle`] rather than pushed into
    /// [`Self::children`]. `sys_wait4` checks this registry for a targeted `pid > 0` wait before
    /// falling back to `children`; `wait4(pid == -1)` ("any child") does NOT consult this
    /// registry -- the scope this pass targets (`beyond_stdio == 0`, one cross-process child at
    /// a time, matching pass 116's "not the general N-child fanout problem" scoping) is a
    /// fork()-then-exec()-then-`waitpid(known_pid)` pattern, which never needs it; a caller doing
    /// `wait(-1)` for a cross-process child is a further, documented gap alongside signal
    /// delivery (see `do_kill`'s doc comment). `do_kill`'s remote-child signal-delivery path
    /// explicitly does NOT check this registry either (out of scope for this pass).
    cross_process_children:
        Mutex<Platform, alloc::vec::Vec<(i32, litebox::platform::CrossProcessChildHandle)>>,
    /// `1` from process creation until this (vforked) process's initial thread either calls
    /// `execve` successfully or exits, `0` otherwise. Only meaningful for a process created via
    /// `vfork()`; a plain `fork()`ed process's `vfork_done` is set immediately and never blocks
    /// anyone. `vfork()`'s POSIX contract requires the calling (parent) thread to be suspended
    /// for exactly this window -- see `do_clone`'s use of this field.
    vfork_done: <Platform as litebox::platform::RawMutexProvider>::RawMutex,
    /// This process's process group ID, as last set via `setpgid()`. Defaults to the process's
    /// own pid at creation, matching real Linux's default (a freshly created process is the
    /// leader of its own, freshly created group). We have no global pid registry (see
    /// `do_kill`'s doc comment on remote pid/tid being unsupported), so `setpgid`/`getpgid` only
    /// ever target the calling process itself -- there is nowhere to look up another process by
    /// pid to move it into a different group.
    pgid: core::sync::atomic::AtomicI32,
    /// This process's process-directed pending-signal queue -- the exact same `Arc` as this
    /// process's own live `Task`'s `SignalState::shared_pending` (see that field's doc comment
    /// on why they must be identical). Reachable from a `Process` handle alone (e.g. via
    /// `children`), unlike the rest of `SignalState`, which lives on `Task` and needs a live
    /// thread context -- this is what lets `do_kill` queue a signal for a live, shim-known
    /// *child* process without needing that child's own `Task` in scope. Actually waking the
    /// child up afterward still goes through this `Process`'s own `inner.threads`/`ThreadRemote`
    /// (see `do_kill`'s remote-child case), which needs no signal-specific plumbing at all --
    /// `ThreadRemote::interrupt` and `has_pending_signals` already existed for exactly this.
    pub(crate) shared_pending: Arc<Mutex<Platform, super::signal::PendingSignals>>,
}

pub(crate) struct Alarm<Platform: ShimPlatform> {
    /// Handle for the alarm timer.
    pub(crate) handle: Option<<Platform as litebox::platform::TimerProvider>::TimerHandle>,
    /// The deadline for the alarm.
    pub(crate) deadline: Option<<Platform as litebox::platform::TimeProvider>::Instant>,
}

impl<Platform: ShimPlatform> Alarm<Platform> {
    /// Returns the time remaining until [`Self::deadline`], or zero if the
    /// alarm is not armed or its deadline has already passed.
    pub(crate) fn remaining(
        &self,
        now: <Platform as litebox::platform::TimeProvider>::Instant,
    ) -> Duration {
        self.deadline
            .as_ref()
            .and_then(|d| d.checked_duration_since(&now))
            .unwrap_or(Duration::ZERO)
    }
}

/// The locked portion of the process state.
struct ProcessInner<Platform: ShimPlatform> {
    /// If true, the whole process is exiting.
    group_exit: bool,
    /// If true, one thread is waiting for other threads to exit.
    is_killing_other_threads: bool,
    /// The exit code of the last exited thread in the process. Not updated once
    /// `group_exit` is set.
    exit_status: ExitStatus,
    /// The thread list for the process, mapped by thread ID.
    threads: BTreeMap<i32, Arc<ThreadRemote<Platform>>>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ExitStatus {
    Exit(i8),
    Signal(litebox_common_linux::signal::Signal),
}

/// Sentinel high byte marking a raw Windows process exit code produced by
/// [`encode_cross_process_exit_status`], distinguishing it from an ordinary, unrelated exit code
/// (e.g. a process that crashed for a reason unrelated to this encoding, or one that was never a
/// `LITEBOX_PROCESS_FORK=1` child at all -- `WaitForSingleObject`/`GetExitCodeProcess` can return
/// any `u32`). Chosen arbitrarily but distinctively; a real guest program's own `exit()` code is
/// always masked to its low 8 bits by Linux itself (see `ExitStatus::Exit(i8)`), so this marker
/// byte can never collide with a genuine encoded guest exit code.
const CROSS_PROCESS_EXIT_MARKER: u32 = 0xC0DE_0000;
const CROSS_PROCESS_EXIT_MARKER_MASK: u32 = 0xFFFF_0000;
const CROSS_PROCESS_EXIT_SIGNAL_FLAG: u32 = 0x0000_8000;

/// Encodes a cross-process `fork()` child's Linux [`ExitStatus`] into a raw Windows process exit
/// code, for that child to pass to `ExitProcess()` on its way down. Pass 141's chosen mechanism
/// for delivering Linux-specific exit detail (`WIFEXITED` vs `WIFSIGNALED`, the exact exit code
/// or signal number) across the OS process boundary WITHOUT needing the child to still be alive
/// enough to send an IPC message -- a bare Windows exit code survives even a hard crash/kill,
/// unlike a message-passing protocol that depends on graceful child-side shutdown code running.
///
/// Layout: high 16 bits are [`CROSS_PROCESS_EXIT_MARKER`] (so the parent can distinguish this
/// from an arbitrary unrelated Windows exit code); bit 15 is set for `Signal`, clear for `Exit`;
/// the low 8 bits hold the exit code (`Exit`) or signal number (`Signal`).
///
/// Not yet called from production code: the actual encode-side call site is a cross-process
/// child's own `sys_exit`/`sys_exit_group` path, which does not exist yet -- `do_clone` still
/// only ever spawns a same-process, thread-based child (see PASS 141 of
/// `scratchpad/jqrepro/FINDINGS.txt`: production wiring is blocked on a second, not-yet-built
/// subsystem, a non-torn-down `CreateProcess` spawn+resume path). This function and
/// [`decode_cross_process_wait_status`] are the proven, ready-to-call codec half of the bridge;
/// `litebox_platform_windows_userland::process_fork::diagnostic_cross_process_wait4_probe`
/// exercises the SAME encoding (duplicated there, not called directly, due to crate layering --
/// `litebox_platform_windows_userland` sits below this crate) against a real child process,
/// live-verified end to end.
#[allow(dead_code, reason = "encode-side production call site (a real spawned child's own exit path) does not exist yet -- see doc comment")]
pub(crate) fn encode_cross_process_exit_status(status: ExitStatus) -> u32 {
    match status {
        ExitStatus::Exit(code) => {
            CROSS_PROCESS_EXIT_MARKER | (u32::from(code.cast_unsigned()) & 0xff)
        }
        ExitStatus::Signal(sig) => {
            CROSS_PROCESS_EXIT_MARKER
                | CROSS_PROCESS_EXIT_SIGNAL_FLAG
                | (sig.as_i32().cast_unsigned() & 0xff)
        }
    }
}

/// Decodes a raw Windows process exit code produced by [`encode_cross_process_exit_status`] back
/// into the Linux [`wait4`](Task::sys_wait4)-style status word the guest expects (the SAME
/// `(exit_code & 0xff) << 8` / `sig & 0x7f` encoding `sys_wait4` already uses for thread-based
/// children -- see that function's body). If `raw_exit_code` does not carry the marker (the
/// child never reached the encoding `ExitProcess` call -- e.g. it was killed by Windows itself,
/// or crashed inside the Windows loader before guest code ever ran), falls back to reporting it
/// as `WIFSIGNALED(SIGKILL)`: the child is definitely gone, and "killed" is a safe, conservative
/// approximation when the real Linux-specific cause cannot be recovered from a bare Windows exit
/// code.
pub(crate) fn decode_cross_process_wait_status(raw_exit_code: u32) -> i32 {
    const SIGKILL: i32 = 9;
    if raw_exit_code & CROSS_PROCESS_EXIT_MARKER_MASK != CROSS_PROCESS_EXIT_MARKER {
        return SIGKILL & 0x7f;
    }
    let low = (raw_exit_code & 0xff).cast_signed();
    if raw_exit_code & CROSS_PROCESS_EXIT_SIGNAL_FLAG != 0 {
        low & 0x7f
    } else {
        (low & 0xff) << 8
    }
}

impl<Platform: ShimPlatform> Process<Platform> {
    /// Creates a new process with the given initial thread and address space.
    ///
    /// `vforked` marks this process as created via `vfork()`: its `vfork_done` starts at `1`
    /// (pending) and the creating parent (see `do_clone`) blocks on it until this process's
    /// initial thread calls `execve` or exits. A plain `fork()`ed process passes `false` and its
    /// `vfork_done` starts already-cleared, so no caller ever blocks on it.
    fn new(
        pid: i32,
        remote: Arc<ThreadRemote<Platform>>,
        pm: litebox::mm::PageManager<Platform, { litebox::mm::linux::PAGE_SIZE }>,
        vforked: bool,
        parent: Option<Weak<Process<Platform>>>,
        shared_pending: Arc<Mutex<Platform, super::signal::PendingSignals>>,
    ) -> Self {
        let nr_threads = <Platform as litebox::platform::RawMutexProvider>::RawMutex::INIT;
        nr_threads.underlying_atomic().store(1, Ordering::Relaxed);
        let vfork_done = <Platform as litebox::platform::RawMutexProvider>::RawMutex::INIT;
        vfork_done
            .underlying_atomic()
            .store(u32::from(vforked), Ordering::Relaxed);
        Self {
            nr_threads,
            inner: Mutex::new(ProcessInner {
                exit_status: ExitStatus::Exit(0),
                group_exit: false,
                is_killing_other_threads: false,
                threads: BTreeMap::from_iter([(pid, remote)]),
            }),
            limits: ResourceLimits::default(),
            alarm_timer: Mutex::new(Alarm {
                handle: None,
                deadline: None,
            }),
            pm,
            parent,
            children: Mutex::new(alloc::vec::Vec::new()),
            cross_process_children: Mutex::new(alloc::vec::Vec::new()),
            vfork_done,
            pgid: core::sync::atomic::AtomicI32::new(pid),
            shared_pending,
        }
    }

    /// Registers `child` as a child of this process, exactly as `do_clone`'s process-clone
    /// branch does. Test-only: production code goes through `do_clone` itself, which has
    /// several other steps (rlimit inheritance, register/TLS translation) around this single
    /// step that a real fork() needs but a test constructing a minimal process family for
    /// signal-delivery testing doesn't.
    #[cfg(test)]
    pub(crate) fn add_child_for_test(&self, pid: i32, child: Arc<Process<Platform>>) {
        self.children.lock().push((pid, child));
    }

    /// Returns every live child of this process whose *own* current `pgid` equals `group` --
    /// used by `do_kill`'s group-directed case (`kill(0|-1|-pgid, sig)`) to reach children that
    /// have been moved into the caller's process group (e.g. via `setpgid()`, the standard
    /// shell-job-control/process-supervisor pattern of putting a whole spawned pipeline into one
    /// group), not just the caller itself.
    pub(crate) fn children_in_group(&self, group: i32) -> alloc::vec::Vec<Arc<Process<Platform>>> {
        self.children
            .lock()
            .iter()
            .filter(|(_, child)| child.pgid.load(Ordering::Relaxed) == group)
            .map(|(_, child)| child.clone())
            .collect()
    }

    /// Registers `handle` as a cross-process `fork()` child of this process (see
    /// `cross_process_children`'s doc comment) -- the cross-process analogue of pushing into
    /// `children`, used by a future `do_clone` cross-process spawn path instead of that push
    /// whenever the new child is a genuinely separate OS process rather than a same-process
    /// thread.
    pub(crate) fn register_cross_process_child(
        &self,
        pid: i32,
        handle: litebox::platform::CrossProcessChildHandle,
    ) {
        self.cross_process_children.lock().push((pid, handle));
    }

    /// Returns the registered [`litebox::platform::CrossProcessChildHandle`] for cross-process
    /// child `pid`, if this process has one -- used by `sys_wait4` to route a wait for such a
    /// child through the real-OS-process wait path instead of reading `children` directly.
    pub(crate) fn find_cross_process_child(
        &self,
        pid: i32,
    ) -> Option<litebox::platform::CrossProcessChildHandle> {
        self.cross_process_children
            .lock()
            .iter()
            .find(|(child_pid, _)| *child_pid == pid)
            .map(|(_, handle)| *handle)
    }

    /// Removes cross-process child `pid` from the registry -- called by `sys_wait4` once that
    /// child has been successfully reaped, mirroring `children`'s own "removed after
    /// successfully waited-for" discipline (Linux does not let you wait for the same child
    /// twice).
    pub(crate) fn reap_cross_process_child(&self, pid: i32) {
        self.cross_process_children.lock().retain(|(p, _)| *p != pid);
    }

    /// Returns the live child `Process` with pid `pid`, if this process has one (see
    /// `children`'s doc comment) -- used by `do_kill`'s remote-child case, the one form of
    /// "signal some other, specific process" this shim can actually reach without a full
    /// shim-wide pid registry.
    pub(crate) fn find_child(&self, pid: i32) -> Option<Arc<Process<Platform>>> {
        self.children
            .lock()
            .iter()
            .find(|(child_pid, _)| *child_pid == pid)
            .map(|(_, child)| child.clone())
    }

    /// Returns this process's parent `Process`, if it is still live (its `Arc` not yet fully
    /// dropped -- i.e. the parent process has not both exited *and* been reaped). `None` for the
    /// bootstrap process (no parent at all) or once the parent is fully gone.
    ///
    /// Used by `Task::prepare_for_exit` to find the reparent target for this process's own
    /// orphaned children; the caller falls back to the shim's bootstrap process when this returns
    /// `None`, approximating real Linux's "reparent to the nearest live ancestor, or PID 1"
    /// behavior. This is a single-hop lookup rather than a full chain walk: once a `Process`'s
    /// `Arc` is fully dropped, its own `parent` field is gone with it, so there is nothing further
    /// to walk through -- the bootstrap fallback covers that case directly instead.
    fn live_parent(&self) -> Option<Arc<Process<Platform>>> {
        self.parent.as_ref()?.upgrade()
    }

    /// Blocks the calling thread until this process's initial thread calls `execve` or exits.
    /// Used by a `vfork()`-ing parent (see `do_clone`) to implement `vfork()`'s POSIX-mandated
    /// parent suspension. Returns immediately (no-op) for a plain `fork()`ed process, whose
    /// `vfork_done` starts already-cleared.
    pub(crate) fn wait_for_vfork_done(&self) {
        loop {
            let v = self.vfork_done.underlying_atomic().load(Ordering::Acquire);
            if v == 0 {
                break;
            }
            let _ = self.vfork_done.block(v);
        }
    }

    /// Marks this (vforked) process's initial thread as having called `execve` or exited,
    /// waking any parent blocked in [`Self::wait_for_vfork_done`]. Idempotent -- safe to call
    /// from both the `execve` and (if `execve` never happens) `exit`/`exit_group` paths.
    fn signal_vfork_done(&self) {
        if self
            .vfork_done
            .underlying_atomic()
            .swap(0, Ordering::Release)
            != 0
        {
            self.vfork_done.wake_all();
        }
    }

    /// Returns the current number of threads in this process.
    pub fn nr_threads(&self) -> u32 {
        self.nr_threads.underlying_atomic().load(Ordering::Relaxed)
    }

    /// Waits for all threads in this process to exit, returning the exit code.
    pub fn wait_for_exit(&self) -> ExitStatus {
        loop {
            let n = self.nr_threads.underlying_atomic().load(Ordering::Acquire);
            if n == 0 {
                break;
            }
            let _ = self.nr_threads.block(n);
        }
        self.inner.lock().exit_status
    }

    /// Interrupts every currently-live thread in this process, causing each to re-evaluate its
    /// wait condition (e.g. pick up a newly pushed pending signal, see `has_pending_signals`) at
    /// its next opportunity. Used by `do_kill`'s remote-child case, after pushing a signal into
    /// this process's own `shared_pending`, to actually wake it up -- mirroring the exact
    /// collect-then-interrupt pattern `exit_group`/`kill_other_threads` already use for
    /// same-process delivery. See `exit_group`'s doc comment on why `interrupt()` must never be
    /// called while still holding `inner` (it can OS-suspend the target thread directly).
    ///
    /// A no-op if every thread has already exited (e.g. the target is a zombie awaiting `wait4`)
    /// -- the pushed signal simply sits unconsumed in `shared_pending` until this `Process` is
    /// eventually dropped, matching real Linux's `kill()` on a zombie: it succeeds but delivers
    /// to nothing.
    pub(crate) fn interrupt_all_threads(&self) {
        let remotes: alloc::vec::Vec<_> = self.inner.lock().threads.values().cloned().collect();
        for thread in remotes {
            thread.interrupt();
        }
    }

    /// Returns the exit code if all threads in this process have already exited, without
    /// blocking. Used by `wait4(WNOHANG)`.
    pub fn try_wait_for_exit(&self) -> Option<ExitStatus> {
        if self.nr_threads.underlying_atomic().load(Ordering::Acquire) == 0 {
            Some(self.inner.lock().exit_status)
        } else {
            None
        }
    }

    /// Attaches a new thread to this process, returning a new remote state for
    /// the thread.
    fn attach_thread(&self, tid: i32) -> Option<Arc<ThreadRemote<Platform>>> {
        // Allocate outside the lock.
        let remote = Arc::new(ThreadRemote::new());
        let mut inner = self.inner.lock();
        if inner.group_exit || inner.is_killing_other_threads {
            return None;
        }
        let old_thread = inner.threads.insert(tid, remote.clone());
        assert!(old_thread.is_none(), "thread ID {tid} already exists");
        let nr_threads = self.nr_threads.underlying_atomic();
        nr_threads.store(nr_threads.load(Ordering::Relaxed) + 1, Ordering::Release);
        Some(remote)
    }

    /// Detaches a thread from this process, WITHOUT waking any `wait4`/`wait_for_exit` waiters
    /// yet. Returns `(notify, process_exited)`: `process_exited` is `true` if this was the
    /// process's last thread (i.e. the whole process has now exited); `notify` indicates whether
    /// [`Self::notify_detached`] must be called once the caller is ready to publish that fact.
    ///
    /// Split from the actual wake-up (`notify_detached`) so that callers who need to perform
    /// process-exit teardown that other processes can observe -- most importantly, releasing this
    /// process's fds (see `close_all_fds_on_process_exit`'s doc comment) -- can do so BEFORE any
    /// `wait4()`-blocked parent is woken and allowed to proceed as though the child were fully
    /// dead. Real Linux's `do_exit()` releases a process's files (`exit_files`) before it becomes
    /// reapable by `wait4()`/appears as a zombie to `release_task()`, so a parent's `wait4()`
    /// returning is always ordered-after every peer (e.g. a pipe reader on the other end of a
    /// closed fd) observing that release. Calling `detach_thread` and waking waiters as a single
    /// atomic step breaks that ordering: a parent's `wait4()` could return (and the shell proceed
    /// to, e.g., read from a pipe expecting EOF, or simply believe the whole pipeline is now fully
    /// quiescent) before this process's fds have actually been released, exposing a shim-only
    /// observable-timing divergence from real Linux on exactly this path.
    ///
    /// # Panics
    /// Panics if the thread ID does not exist in this process.
    fn detach_thread(&self, tid: i32) -> (bool, bool) {
        let data;
        let (notify, process_exited) = {
            let mut inner = self.inner.lock();
            data = inner.threads.remove(&tid);
            assert!(data.is_some());

            let nr_threads = self.nr_threads.underlying_atomic();
            let n = nr_threads.load(Ordering::Relaxed);
            let new_count = n.checked_sub(1).expect("decrementing from zero threads");
            nr_threads.store(new_count, Ordering::Release);
            litebox_util_log::debug!(
                tid:% = tid,
                new_count:% = new_count,
                is_killing_other_threads:% = inner.is_killing_other_threads;
                "detach_thread: decremented nr_threads"
            );
            if new_count == 0 {
                assert!(inner.threads.is_empty());
                // The last thread exited. Prevent new threads.
                inner.group_exit = true;
                // Cover the case of a vfork()'d process exiting without ever calling execve --
                // otherwise a parent blocked in `wait_for_vfork_done` would hang forever. A
                // no-op if `signal_vfork_done` already ran from a successful `execve`.
                self.signal_vfork_done();
            }

            // Notify waiters if this is the last thread of the process
            // (`wait_for_exit`) or if this is the last thread being killed
            // during an exec (`kill_other_threads`).
            (
                new_count == 0 || (new_count == 1 && inner.is_killing_other_threads),
                new_count == 0,
            )
        };
        litebox_util_log::debug!(tid:% = tid, notify:% = notify, process_exited:% = process_exited; "detach_thread: notify decision");
        (notify, process_exited)
    }

    /// Wakes any `wait4`/`wait_for_exit` waiters previously deferred by [`Self::detach_thread`].
    /// Must only be called with the `notify` value that call returned, after any process-exit
    /// teardown (fd release, etc.) that must be externally observable before waiters proceed has
    /// completed -- see `detach_thread`'s doc comment for why the split exists.
    fn notify_detached(&self) {
        self.nr_threads.wake_all();
    }

    /// Takes and returns every remaining (not-yet-waited-for) child of this process, leaving its
    /// own child list empty.
    ///
    /// Used by `Task::prepare_for_exit` to reparent this process's children elsewhere once this
    /// process itself has fully exited -- see that function's doc comment for why this is
    /// necessary.
    fn take_children(&self) -> alloc::vec::Vec<ChildEntry<Platform>> {
        core::mem::take(&mut *self.children.lock())
    }

    /// Appends `orphans` to this process's child list, so a later `wait4(-1, ...)`/`wait4(pid,
    /// ...)` from this process can reap them.
    ///
    /// Used by `Task::prepare_for_exit` to reparent an exiting process's still-running children
    /// onto the shim's bootstrap (PID-1-equivalent) process -- see that function's doc comment.
    fn adopt_children(&self, orphans: alloc::vec::Vec<ChildEntry<Platform>>) {
        if orphans.is_empty() {
            return;
        }
        self.children.lock().extend(orphans);
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Updates the process exit status for a thread exit.
    fn exit_thread(&self, code: i8) {
        litebox_util_log::debug!(tid:% = self.tid, code:% = code; "sys_exit: exit_thread entry");
        let mut inner = self.thread.process.inner.lock();
        if self.is_exiting() {
            litebox_util_log::debug!(tid:% = self.tid; "sys_exit: already exiting, no-op");
            return;
        }
        inner.exit_status = ExitStatus::Exit(code);
        self.thread.remote.is_exiting.store(true, Ordering::Relaxed);
        litebox_util_log::debug!(tid:% = self.tid; "sys_exit: is_exiting set, thread will unwind to prepare_for_exit");
    }

    /// Updates the process exit status for a group exit and signals all threads
    /// to exit.
    pub(crate) fn exit_group(&self, status: ExitStatus) {
        litebox_util_log::debug!(tid:% = self.tid, status:? = status; "sys_exit_group: entry");
        // Mark every thread as exiting, and collect their remotes, while holding `inner` --
        // but do NOT call `interrupt()` (below) while still holding it.
        //
        // `ThreadRemote::interrupt`/`ThreadHandle::interrupt` can call the platform's
        // `SuspendThread` (Windows) directly on another live thread (see
        // `litebox::event::wait::ThreadHandle::interrupt` and its platform-level
        // `interrupt_thread`/VEH counterpart) -- an OS-level thread-suspend primitive that has
        // no notion of "don't suspend while a Rust-level lock is held" by the *target* thread.
        // If any other thread in this process is concurrently trying to acquire this same
        // `inner` lock (e.g. another thread racing through its own `exit_thread`/`detach_thread`,
        // or `attach_thread` for a just-`clone()`d sibling), holding `inner` across the whole
        // interrupt loop serializes that thread's progress behind this loop for no reason, and
        // -- worse -- widens the window in which a suspended thread can be frozen while it
        // legitimately needs this exact lock to make forward progress once resumed, needlessly
        // extending how long shutdown can appear stalled. This mirrors the exact class of bug
        // fixed in `write_to_raw_handle`'s doc comment (`litebox_platform_windows_userland`):
        // never hold a Rust-level lock across a call that can suspend another thread. Collecting
        // the remotes first and interrupting after dropping the lock removes that coupling
        // entirely, matching how a real kernel's `do_group_exit` never needs to hold a
        // process-wide lock while signaling sibling threads.
        let remotes: alloc::vec::Vec<_> = {
            let mut inner = self.thread.process.inner.lock();
            if self.is_exiting() {
                return;
            }
            assert!(!inner.group_exit);
            inner.exit_status = status;
            inner.group_exit = true;
            for thread in inner.threads.values() {
                thread.is_exiting.store(true, Ordering::Relaxed);
            }
            inner.threads.values().cloned().collect()
        };
        litebox_util_log::debug!(
            tid:% = self.tid,
            n_remotes:% = remotes.len();
            "sys_exit_group: interrupting sibling threads"
        );
        for thread in remotes {
            thread.interrupt();
        }
        litebox_util_log::debug!(tid:% = self.tid; "sys_exit_group: done interrupting siblings");
    }

    /// Kills all other threads in the process, waiting for them to exit.
    ///
    /// Returns false if this thread is already exiting.
    #[must_use]
    fn kill_other_threads(&self) -> bool {
        // See `exit_group`'s doc comment on why `interrupt()` must never be called while
        // holding `inner`: collect the other threads' remotes first, mark
        // `is_killing_other_threads` and release the lock, then interrupt them afterward.
        let remotes: alloc::vec::Vec<_> = {
            let mut inner = self.thread.process.inner.lock();
            if self.is_exiting() {
                return false;
            }
            let remotes = inner
                .threads
                .iter()
                .filter(|&(&tid, _)| tid != self.tid)
                .map(|(_, thread)| thread.clone())
                .collect::<alloc::vec::Vec<_>>();
            for thread in &remotes {
                thread.is_exiting.store(true, Ordering::Relaxed);
            }
            assert!(!inner.is_killing_other_threads);
            inner.is_killing_other_threads = true;
            remotes
        };
        litebox_util_log::debug!(
            tid:% = self.tid,
            n_remotes:% = remotes.len();
            "kill_other_threads: interrupting siblings"
        );
        for thread in &remotes {
            thread.interrupt();
        }
        // Wait for other threads to exit.
        loop {
            let n = self
                .thread
                .process
                .nr_threads
                .underlying_atomic()
                .load(Ordering::Acquire);
            litebox_util_log::debug!(tid:% = self.tid, n:% = n; "kill_other_threads: nr_threads check");
            if n == 1 {
                break;
            }
            let _ = self.thread.process.nr_threads.block(n);
        }
        self.thread.process.inner.lock().is_killing_other_threads = false;
        litebox_util_log::debug!(tid:% = self.tid; "kill_other_threads: done");
        true
    }

    /// Returns true if the task is exiting and should not continue running
    /// guest code.
    pub fn is_exiting(&self) -> bool {
        self.thread.remote.is_exiting.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
enum ThreadInitState {
    #[default]
    None,
    NewProcess(crate::loader::elf::ElfLoadInfo),
    NewThread {
        stack: Option<usize>,
        tls: Option<ThreadLocalDescriptor>,
        set_child_tid: Option<UserPtrMut<i32>>,
    },
    /// `fork()`/`vfork()`: the child resumes execution as if it were the parent returning from
    /// the same `clone()` syscall, with an identical register state except `rax` (the syscall
    /// return value), which is 0 in the child (vs. the child's pid in the parent).
    ///
    /// Also carries the parent's `FsBase` (the platform's per-host-thread FS-segment-base
    /// register, which backs the guest's TLS pointer): a `fork()`-created child runs on a
    /// brand-new host thread, whose FS base starts unset, but the guest process's TLS block
    /// (set up by libc at process startup, well before this `fork()` call) lives at a fixed
    /// guest address that both parent and child must keep dereferencing identically -- without
    /// explicitly propagating it here, the child's first guest instruction that touches `%fs`
    /// (which libc's own post-`clone()` return path does immediately, e.g. to check TLS-stored
    /// cancellation/errno state) dereferences FS base 0 and faults.
    ///
    /// Also carries the `fork()` address-space relocation map, so the platform can verify the
    /// child's post-`fork()` execution against it (detecting stale, untranslated pointers into
    /// the parent's address space before they silently corrupt the still-running parent). See
    /// [`litebox::platform::ForkChildVerificationProvider`].
    ForkedChild(
        litebox_common_linux::PtRegs,
        usize,
        alloc::sync::Arc<litebox::mm::AddressRelocations>,
    ),
}

/// Credentials of a process
#[derive(Clone)]
pub(crate) struct Credentials {
    pub uid: u32,
    pub euid: u32,
    pub gid: u32,
    pub egid: u32,
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    pub(crate) fn process(&self) -> &Arc<Process<Platform>> {
        &self.thread.process
    }

    /// Set the current task's command name.
    pub(crate) fn set_task_comm(&self, comm: &[u8]) {
        let mut new_comm = [0u8; litebox_common_linux::TASK_COMM_LEN];
        let comm = &comm[..comm.len().min(litebox_common_linux::TASK_COMM_LEN - 1)];
        new_comm[..comm.len()].copy_from_slice(comm);
        self.comm.set(new_comm);
    }

    /// Handle syscall `prctl`.
    pub(crate) fn sys_prctl(&self, arg: PrctlArg) -> Result<usize, Errno> {
        match arg {
            PrctlArg::GetName(name) => name
                .write_slice_at_offset::<Platform>(0, &self.comm.get())
                .ok_or(Errno::EFAULT)
                .map(|()| 0),
            PrctlArg::SetName(name) => {
                let mut name_buf = [0u8; litebox_common_linux::TASK_COMM_LEN - 1];
                // strncpy
                for (i, byte) in name_buf.iter_mut().enumerate() {
                    let b = name
                        .read_at_offset::<Platform>(isize::try_from(i).unwrap())
                        .ok_or(Errno::EFAULT)?;
                    if b == 0 {
                        break;
                    }
                    *byte = b;
                }
                self.set_task_comm(&name_buf);
                Ok(0)
            }
            PrctlArg::CapBSetRead(cap) => {
                // Return 1 if the capability specified in cap is in the calling
                // thread's capability bounding set, or 0 if it is not.
                if cap
                    > litebox_common_linux::CapSet::LAST_CAP
                        .bits()
                        .trailing_zeros() as usize
                {
                    return Err(Errno::EINVAL);
                }
                // Note we don't support capabilities in LiteBox, so we always return 0.
                Ok(0)
            }
            _ => unimplemented!(),
        }
    }

    /// Handle syscall `arch_prctl`.
    pub(crate) fn sys_arch_prctl(&self, arg: ArchPrctlArg) -> Result<(), Errno> {
        match arg {
            #[cfg(target_arch = "x86_64")]
            ArchPrctlArg::SetFs(addr) => self
                .global
                .platform
                .set_arch_specific_register(&ArchSpecificRegister::FsBase, addr)
                .map_err(Errno::from),
            #[cfg(target_arch = "x86_64")]
            ArchPrctlArg::GetFs(addr) => {
                let fsbase = self
                    .global
                    .platform
                    .get_arch_specific_register(&ArchSpecificRegister::FsBase)?;
                addr.write_at_offset::<Platform>(0, fsbase)
                    .ok_or(Errno::EFAULT)?;
                Ok(())
            }
            ArchPrctlArg::CETStatus | ArchPrctlArg::CETDisable | ArchPrctlArg::CETLock => {
                Err(Errno::EINVAL)
            }
            _ => unimplemented!(),
        }
    }
}

const ROBUST_LIST_LIMIT: isize = 2048;

/// Bit set in a robust futex word's low bits by the kernel (here, the shim) when the thread
/// that held the lock dies without releasing it, so the next owner can detect the previous
/// holder died mid-critical-section. Matches Linux's `FUTEX_OWNER_DIED`.
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
/// Bit set in a robust futex word's low bits when at least one thread is (or might be) sleeping
/// in `FUTEX_WAIT` on it, so the unlocker knows to `FUTEX_WAKE`. Matches Linux's `FUTEX_WAITERS`.
const FUTEX_WAITERS: u32 = 0x8000_0000;
/// Mask isolating the TID stored in a robust futex word's low bits. Matches Linux's
/// `FUTEX_TID_MASK`.
const FUTEX_TID_MASK: u32 = 0x3fff_ffff;

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Process a single robust-futex-list entry belonging to a dying thread: if the futex word
    /// still records this thread as the owner, mark it as dead (setting [`FUTEX_OWNER_DIED`] and
    /// clearing the TID) and, if any waiters may be present, wake one -- mirroring Linux's
    /// `handle_futex_death` (`kernel/futex/core.c`), which exists so a thread that dies while
    /// holding a robust `pthread_mutex_t` doesn't leave every future waiter blocked forever on a
    /// lock whose owner will never call `FUTEX_WAKE` again.
    ///
    /// Previously this was an unconditional `todo!()`: any thread whose robust list contained
    /// even one entry at exit (e.g. a libuv/V8 worker thread that happened to hold, or was
    /// mid-way through locking/unlocking, a robust mutex when it exited) would panic partway
    /// through `prepare_for_exit`'s teardown -- after `detach_from_process` had already run and
    /// decremented `nr_threads`, but before this dying thread ever issued the `FUTEX_WAKE` a
    /// sibling thread blocked on that same lock (e.g. the main thread, in `FUTEX_WAIT` with the
    /// glibc/musl "locked, has waiters" futex-word value `2`) was relying on -- permanently
    /// stranding that waiter (this is the real, reproduced `node -e "console.log(1)"`
    /// intermittent-hang root cause this fixes, not a hypothetical).
    fn handle_futex_death(&self, futex_addr: UserPtr<u32>, _pending_op: bool) -> Result<(), Errno> {
        if !futex_addr.as_usize().is_multiple_of(4) {
            return Err(Errno::EINVAL);
        }
        let futex_addr = UserPtrMut::from_usize(futex_addr.as_usize());

        let Some(word) = futex_addr.read_at_offset::<Platform>(0) else {
            return Err(Errno::EFAULT);
        };

        // Only touch the word if it's still (nominally) owned by this dying thread -- a lock
        // that was already unlocked and re-acquired by someone else, or never actually locked by
        // us despite being linked into our robust list (see `list_op_pending`'s doc comment),
        // must be left alone.
        #[expect(
            clippy::cast_sign_loss,
            reason = "tid is always non-negative; only ever compared against another tid read \
                      back from a futex word, never used arithmetically"
        )]
        if (word & FUTEX_TID_MASK) != self.tid as u32 {
            return Ok(());
        }

        let had_waiters = word & FUTEX_WAITERS != 0;
        let new_word = (word & FUTEX_WAITERS) | FUTEX_OWNER_DIED;
        if futex_addr
            .write_at_offset::<Platform>(0, new_word)
            .is_none()
        {
            return Err(Errno::EFAULT);
        }

        if had_waiters {
            let _ = self.sys_futex(FutexArgs::Wake {
                addr: futex_addr,
                flags: litebox_common_linux::FutexFlags::PRIVATE,
                count: 1,
            });
        }
        Ok(())
    }
}

fn fetch_robust_entry(
    head: UserPtr<litebox_common_linux::RobustList>,
) -> (UserPtr<litebox_common_linux::RobustList>, bool) {
    let next = head.as_usize();
    (UserPtr::from_usize(next & !1), next & 1 != 0)
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    fn wake_robust_list(
        &self,
        head: UserPtr<litebox_common_linux::RobustListHead>,
    ) -> Result<(), Errno> {
        let mut limit = ROBUST_LIST_LIMIT;
        let head_ptr = head.as_usize();
        let head = head.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        let (mut entry, _pi) = fetch_robust_entry(UserPtr::from_usize(head.list.next));
        let (pending, _ppi) = fetch_robust_entry(UserPtr::from_usize(head.list_op_pending));
        let futex_offset = head.futex_offset;
        let entry_head = head_ptr + offset_of!(litebox_common_linux::RobustListHead, list);
        while entry.as_usize() != entry_head && limit > 0 {
            let nxt = entry
                .read_at_offset::<Platform>(0)
                .map(|e| fetch_robust_entry(UserPtr::from_usize(e.next)));
            if entry.as_usize() != pending.as_usize() {
                self.handle_futex_death(
                    UserPtr::from_usize(entry.as_usize() + futex_offset),
                    false,
                )?;
            }
            let Some((next_entry, _next_pi)) = nxt else {
                return Err(Errno::EFAULT);
            };

            entry = next_entry;
            limit -= 1;
        }

        if pending.as_usize() != 0 {
            let _ = self
                .handle_futex_death(UserPtr::from_usize(pending.as_usize() + futex_offset), true);
        }
        Ok(())
    }
}

/// Fixes up stale, untranslated pointers copied verbatim onto a `fork()` child's own (already
/// correctly relocated) memory -- specifically, the small, bounded set of return addresses and
/// spilled registers that libc's own post-`fork()`/`clone()` unwind (musl's `_Fork` -> `fork` ->
/// the guest's caller) reads back out of stack slots written by `call` instructions *before* this
/// `fork()` happened.
///
/// Scans 8-byte-aligned slots across ONLY the bounded window near `rsp` of the child's own stack
/// region -- no other tracked region, including a sibling entry that may hold the TCB (see
/// "Excluding everything but the stack" below for why not).
///
/// # Excluding everything but the stack
///
/// A `call`-instruction return address or a spilled callee-saved register can never legitimately
/// live in the heap, the guest's loaded code, `.data`/`.bss`/GOT, a TCB placed in its own separate
/// mapping, or any other region besides the stack itself -- so those were never a source of
/// genuine stale pointers this pass needs to fix, only a false-positive hazard: any 8-byte-aligned
/// value there that numerically happens to fall inside some other mapping's address range gets
/// misidentified as a stale pointer and silently corrupted. This is far more dangerous than it
/// sounds for two of those regions specifically -- a genuine function pointer stored in
/// `.data`/GOT/a TCB field, later called or jumped through, corrupts control flow the moment it
/// resolves to a wrong-but-plausible-looking address; a byte inside loaded code corrupts a decoded
/// instruction directly, which can decode as an undefined or even privileged opcode. Both were
/// directly observed and root-caused via this pass's own diagnostic tracing: with the heap
/// excluded but every other region still scanned in full (this pass's original scope), a
/// 4-`ls`-in-one-shell repro (`sh -c "ls /; ls /usr; ls /tmp; ls /bin | head -3"`) reliably (100%
/// of runs) crashed the child on a `STATUS_PRIVILEGED_INSTRUCTION` (`0xC0000096`) Win32 exception a
/// few instructions into its very first post-`fork()` syscall, well before `execve()`; excluding
/// only executable ranges from the broader scan still reproduced the same crash 100% of the time
/// (leaving `.data`/`.bss`/GOT scanned in full still corrupted a function pointer that later got
/// jumped through, landing execution on unrelated, non-instruction bytes), and a follow-up attempt
/// to keep scanning small, proximate "sibling" regions of the stack (to preserve TCB coverage)
/// still reproduced the crash too, empirically confirmed by the same repro -- so this pass now
/// scans the stack's own region alone. A prior version of this pass additionally scanned every
/// sibling entry to reach `struct pthread`'s TCB fields (its self-referential `%fs:0` word, its
/// cached `stack`/`stack_size`, ...) placed adjacent to the stack by `Vmem::duplicate`'s grouping;
/// the ABI-mandated self-referential `%fs:0` pointer specifically is already independently and
/// exactly corrected by `sys_clone`'s own `fs_base` translation (see this function's caller), so
/// narrowing this pass to the stack alone does not reopen that one case, though any *other* stale
/// TCB field this pass previously happened to also fix up is no longer covered -- no live repro of
/// that narrower gap is known; if one surfaces, prefer fixing the specific TCB field(s) musl's
/// unwind actually reads rather than reopening a broad scan of the whole sibling region.
///
/// # Bounding the stack scan to a window above `rsp`
///
/// A guest's *stack* can easily be hundreds of kilobytes deep by the time it reaches a `fork()`
/// call several frames down a shell's command-parsing/expansion code (confirmed live: a guest
/// shell's `stalloc`-style stack-string arena used to build up a command's `argv`/pathname text
/// one byte at a time lives *on the stack*, not the heap, roughly 150KB below `rsp` at `fork()`
/// time in one observed repro) -- scanning the *entire* stack range hits the same false-positive
/// hazard described above: ordinary in-progress program data (partially-built strings, local
/// variables) that happens to numerically fall within the parent's address range gets
/// misidentified as a stale pointer and silently corrupted. But unlike the rest of the stack, the
/// portion right above `rsp` genuinely does contain the real stale pointers this pass exists to
/// fix -- they are, by construction, always *shallow* (within the handful of frames libc's own
/// `fork()`/`clone()` unwind touches immediately after `fork()` returns, before the child has
/// executed a single guest instruction of its own). So only the bounded window
/// `[child_rsp - STACK_SCAN_MARGIN, region_top)` is scanned, never the whole region.
///
/// # `STACK_SCAN_MARGIN` was too wide: a real false-positive hazard, though NOT the full story
///
/// The original 64KB margin was chosen only to comfortably clear the "a handful of frames" depth
/// this pass actually needs, without any live evidence pinning down how much of that 64KB was
/// truly harmless to sweep. It was not: instrumenting this pass during a live repro (`apk add
/// nodejs` then `node --version`, and independently a plain `busybox echo --version` after
/// heap-churning prior fork/exec commands) showed it performing THOUSANDS of "heals" per single
/// `fork()` -- not the "small, deterministic, always-present set of slots" this pass's top-level
/// doc comment claims -- each one an 8-byte-aligned stack word that happened to hold an ordinary,
/// live, in-progress `stalloc`-arena value which ALSO happened to look like a translatable stale
/// pointer (musl/ash's stack-string arena stores plenty of genuine stack-address-shaped pointers
/// as part of normal operation, e.g. nested argv/offset bookkeeping, not just call-stack return
/// addresses). Narrowing the margin to a size that only covers libc's own unwind depth (a handful
/// of frames, not tens of KB of live shell-arena data) closes MOST of that surface without
/// reopening the crash this pass exists to fix (confirmed via the same 4-`ls` repro, 15/15 clean
/// after this change) -- but a live repro of the argv-corruption bug (`--version`'s NUL terminator
/// corrupted) STILL reproduced (~38/40) even with this narrower margin, meaning this is a real,
/// independently worthwhile hardening but NOT, by itself, the fix for that bug -- its root cause
/// was not fully pinned down in the investigation that produced this comment. If investigating
/// further: the corrupted string's address in every observed case was on a small (~12KB), heavily
/// churned worker-thread-style stack region, always right after "busybox"/"echo"-style argv
/// entries at consistent small offsets, suggesting the true mechanism may not be this proactive
/// scan at all (it reproduces identically whether this scan's margin is 64KB or 4KB) but something
/// else entirely in the `fork()`/`execve()` path -- possibly in how the guest's own allocator
/// (mallocng) or stack-arena bookkeeping interacts with litebox's `brk`/mmap emulation, rather than
/// a memory-scanning false positive.
///
/// # Root cause found and fixed: an executable-range filter on the healed VALUE, not just the scan window
///
/// The "38/40" residual from the margin-narrowing round above was root-caused in a later round via
/// a deterministic repro: in a fresh interactive shell, whether `ls /`'s `argv` gets corrupted
/// depends ENTIRELY on the exact character length of the immediately preceding command (`echo
/// <payload>`) -- clean for total command length 6-12 and 29+, corrupt 100% of the time for total
/// length 13-28 (payload 8-23 chars), reproduced 16/16 lengths and confirmed 10/10 repeatable at
/// several lengths within the corrupt band. This is exactly the false-positive hazard the sections
/// above already predicted, just pinned to its precise trigger: ash's `stalloc`-arena bookkeeping
/// (nested argv/offset pointers, genuinely stack-address-shaped as ordinary DATA) shifts in a
/// length-dependent way as the shell parses/expands the next command line, so for some lengths a
/// live arena slot that merely *numerically* falls in the parent's stack range -- but is not a
/// pointer at all, let alone a return address -- lands inside the `STACK_SCAN_MARGIN` window and
/// gets misidentified and blindly overwritten by `AddressRelocations::translate`'s exact-range-
/// membership check, which (as this whole investigation has repeatedly rediscovered for the heap
/// and `.data` cases) cannot by itself distinguish a genuine stale pointer from ordinary data that
/// coincidentally shares its address range.
///
/// The fix: in addition to falling in the scan window and range-translating, a slot is only healed
/// if the TRANSLATED value also lands in a DESTINATION range that was executable in the source
/// address space (`AddressRelocations::is_in_destination_executable_range`, already used
/// elsewhere in this module for exactly this kind of structural, not-heuristic-guessing check). A
/// genuine `call`-instruction return address always points into code; ash's arena bookkeeping never
/// does. This is precise for the return-address class this pass exists to fix (per its top-level
/// doc comment, "a handful of frames, well under a dozen call/ret pairs") and, unlike a value-shape
/// heuristic, cannot misfire on data that happens to look pointer-shaped -- only a value that is
/// BOTH an exact address-range match AND lands in code passes. It deliberately narrows coverage
/// versus the pre-fix pass in one respect: a spilled callee-saved register value that points at
/// non-code memory (e.g. a stack slot literally holding a heap or `.data` pointer, not a return
/// address) is no longer healed here -- believed acceptable because every currently-known live
/// register at the fork() boundary is already translated directly from `child_ctx` before this pass
/// ever runs (see this function's caller), and no repro of a stack-RESIDENT non-code stale pointer
/// (as opposed to a return address) has ever been observed across this investigation's many rounds.
///
/// Verified: the full length-sweep repro (payload 1-40, one run each) is 40/40 clean after this
/// fix (was 17 corrupt lengths, spanning payload 8-23 plus an unrelated one-off at 40, before it);
/// 10/10 repeat runs clean at payload lengths 10/15/20/23 (all four squarely inside the former
/// corrupt band); the pre-existing `sh -c "ls /; ls /usr; ls /tmp; ls /bin | head -3"`
/// `STATUS_PRIVILEGED_INSTRUCTION` crash-regression repro remained 20/20 clean (no reopening of the
/// `residual-second-fork-verify-corruption-bug`/`fixup_stale_elf_data_pointers` fix above, which
/// this change does not touch). Do not treat this as license to re-litigate the executable-range
/// filter's soundness for the register class it does not cover without a live repro in hand first.
#[cfg(target_arch = "x86_64")]
fn fixup_stale_stack_pointers<Platform: ShimPlatform>(
    relocations: &litebox::mm::AddressRelocations,
    child_rsp: usize,
) {
    // Upper bound on how deep libc's own fork()/clone() unwind reads stale spilled
    // registers/return addresses from -- musl's _Fork -> fork -> caller is a handful of stack
    // frames (observed via disassembly: well under a dozen `call`/`ret` pairs, each frame
    // typically well under 256 bytes on x86-64). 4KB comfortably covers dozens of such frames
    // with wide margin, while excluding the tens-of-KB-deep live shell-arena data a wider margin
    // was found to corrupt (see "STACK_SCAN_MARGIN was too wide" above) -- any stale pointer this
    // narrower window misses is still caught reactively by `fork_verify`'s single-step healing
    // (see that module's doc comment) for the remainder of the fork()-to-execve() window.
    const STACK_SCAN_MARGIN: usize = 4 * 1024;
    for (source_range, dest_base) in relocations.ranges() {
        let dest_base = *dest_base;
        let dest_top = dest_base + source_range.len();
        // Only the single tracked region that contains `child_rsp` -- the child's own stack -- is
        // ever scanned, and only the bounded window near `rsp` within it. See this function's
        // top-level doc comment for why every other region (a sibling TCB/guard-page entry
        // included) is excluded even though `duplicate()` may place the real TCB in a sibling
        // entry of the same relocation group: the self-referential `%fs:0` TCB pointer this pass
        // would otherwise also fix up there is already independently corrected by `sys_clone`'s
        // own `fs_base` translation (see this function's caller), which is exact rather than
        // heuristic, so this pass narrowing to the stack alone does not reopen that case.
        if !(dest_base..dest_top).contains(&child_rsp) {
            continue;
        }
        let scan_start = child_rsp.saturating_sub(STACK_SCAN_MARGIN).max(dest_base);
        let mut addr = scan_start;
        while addr < dest_top {
            let slot = UserPtrMut::<usize>::from_usize(addr);
            if let Some(value) = slot.read_at_offset::<Platform>(0)
                && let Some(translated) = relocations.translate(value)
                // A genuine `call`-instruction return address always points into executable
                // memory (specifically, just past a `call` site) -- see "Length-dependent
                // false positives" below for why this extra check, not just range membership,
                // is required. Ordinary shell-arena data that merely looks stack-address-shaped
                // never satisfies this, since it does not point into code.
                //
                // A saved STACK pointer (not a return address) is the other narrow, precise class
                // this pass also heals: musl's `_Fork` spills its own live `rsp` into a scratch
                // slot (its `rt_sigprocmask`/signal-mask-restore bookkeeping) before recursing
                // deeper, and reloads it directly into RSP on the way back out -- observed live via
                // `fork_verify`'s single-step diagnostic reloading a raw, untranslated PARENT-stack
                // address straight into RSP with no intervening `call`/`ret`. Unlike the
                // shell-arena false positives the executable-range check above exists to reject,
                // this class is safe to identify by exact-range membership alone: a value that
                // both (a) exactly matches a live parent-stack address (`translate` already
                // requires this) AND (b) falls within the child's OWN translated stack region
                // (`dest_base..dest_top`, the same narrow, single-region bound this whole scan is
                // already limited to) cannot be ordinary non-pointer shell data, because shell
                // stack-arena bookkeeping never stores an absolute pointer into the interpreter's
                // OWN call stack -- only a genuinely saved stack pointer does.
                //
                // A HEAP pointer spilled to the stack by musl's own `_Fork`/`fork` unwind is the
                // third narrow, precise class this pass heals (pass 87, `queue()`'s double-null
                // assert in mallocng's `nontrivial_free`: `ctx.active[sc]`, a `struct meta*`,
                // register-spilled into this exact bounded window and left untranslated,
                // compared against a correctly-translated pointer, spuriously failing to compare
                // equal and re-queuing an already-queued node). This is NOT the sweep-narrowing
                // heuristic three prior attempts reverted (see
                // `AddressRelocations::private_data_ranges_excluding_anonymous_mmap`'s doc
                // comment): that class scanned the HEAP itself -- megabytes of dense,
                // allocator-owned bitmasks/indices/small integers where `translate`'s exact
                // range-membership match can still coincidentally fire on ordinary payload data
                // that numerically falls in some *other* tracked range's wide span. This check
                // scans only the SAME already-bounded `STACK_SCAN_MARGIN` window (a handful of
                // libc unwind frames, not live payload/arena data -- see this function's
                // top-level doc comment for why that window is safe to scan at all), and requires
                // the translated value to land specifically in `is_in_destination_heap_range`
                // (musl's mallocng slab/meta objects), not merely somewhere in the wide private
                // data range. A libc unwind frame's spilled scratch slots hold pointers a
                // fork()-in-progress call chain is actively using (allocator bookkeeping among
                // them) or nothing at all; ordinary integers this shallow in the unwind have no
                // reason to coincide with a live heap object's address any more than a return
                // address coincides with code by chance.
                && (relocations.is_in_destination_executable_range(translated)
                    || (dest_base..dest_top).contains(&translated)
                    || relocations.is_in_destination_heap_range(translated))
            {
                let _ = slot.write_at_offset::<Platform>(0, translated);
            }
            addr += core::mem::size_of::<usize>();
        }
        // Exactly one tracked region can contain `child_rsp`.
        break;
    }
}

/// Translate every stale, untranslated SOURCE-space pointer stored in the `fork()` child's copy of
/// each loaded ELF image's *writable data segment* (`.data`/`.got`/`.data.rel.ro`/`.bss`) into its
/// DESTINATION equivalent.
///
/// # The bug this fixes
///
/// Real Linux `fork()` gives the child the parent's exact virtual addresses, so absolute pointers
/// the parent stored in memory stay valid verbatim. LiteBox cannot: parent and child share one
/// host process, so the child's memory must be relocated (see `PageManager::duplicate`'s "Known
/// deviation" section). RIP-relative references survive that relocation -- `duplicate` moves each
/// ELF image's segments as one coherent group, preserving their relative offsets exactly --
/// but every *absolute* pointer stored in guest memory is left pointing at the parent.
///
/// A loaded ELF's writable data segment is where those absolute pointers live: every
/// `R_X86_64_RELATIVE`/`RELR` slot the loader filled in with `load_base + addend`, plus every
/// global pointer variable the program assigns at runtime. Left stale, they do not fault (the
/// parent's mappings are still mapped in the same host process) -- they silently read the
/// *parent's* copy of the object, or get mistaken for something else entirely.
///
/// Confirmed live as the cause of this investigation's long-standing `STATUS_PRIVILEGED_
/// INSTRUCTION` crash (`.gm/prd.yml`'s `residual-second-fork-verify-corruption-bug`): busybox
/// `ash` keeps its file-stack head in `.data`, initialized by a `RELR` relocation to the address
/// of a static sentinel node in `.bss`. Its pop loop reads that head and compares it against
/// `leaq sentinel(%rip)`:
///
/// ```text
///   leaq  0x84431(%rip), %rax   ; -> the CHILD's sentinel address (RIP-relative: correct)
///   movq  0x83e40(%rip), %rbx   ; -> the head slot: still the PARENT's sentinel address (stale)
///   cmpq  %rax, %rbx
///   je    <done>                ; never taken in the child
///   ...
///   movq  %rbx, %rdi
///   callq *free@GOT             ; the static sentinel gets handed to free()
/// ```
///
/// so the loop ran past its terminator and passed a `.bss` object to `free()`. musl's mallocng
/// correctly rejected the misaligned non-heap pointer with its deliberate `hlt` alignment assert,
/// which surfaces on Windows as `0xC0000096`. Translating the head slot here makes the comparison
/// match, exactly as it does on Linux.
///
/// # Why scanning these ranges is precise, not the heuristic scan that was reverted before
///
/// Sweeping memory for "values that fall in a parent range" is only sound where ordinary program
/// data cannot live: `AddressRelocations::translate` cannot tell a genuine stale pointer from a
/// string or integer that coincidentally lands in the parent's address range. An earlier
/// whole-heap sweep did exactly that and corrupted a shell's stack-string arena (see
/// `fixup_stale_stack_pointers`'s doc comment and `AddressRelocations::heap_range`).
///
/// `AddressRelocations::private_data_ranges` avoids that by construction rather than by guess: a
/// range qualifies only if it is simultaneously private (excluding any `MAP_SHARED` mapping,
/// which duplication itself already rejects), writable, non-executable, and not the stack (which
/// is where all bulk program data lives -- covered separately, and only within a bounded window,
/// by `fixup_stale_stack_pointers`). A loaded ELF's `PF_W` `PT_LOAD` segment and the `brk` heap
/// both satisfy this and both are swept: an image's globals are pointers, counters and flags,
/// never buffers, so a whole-word scan cannot collide with live payload data there; the heap is
/// covered too (see the next section for why that is safe despite holding live payload data too).
///
/// # The heap: included here, and why that is NOT the same hazard as a byte-pattern scan
///
/// An earlier revision of this pass excluded the heap, based on a live repro (`apk add nodejs`
/// then `node --version`) that appeared to show a heap-scanning pass corrupting an argv string's
/// NUL terminator. That repro's actual cause was a *different*, since-removed heuristic that
/// existed at the same time: a byte-pattern "does this value look like a pointer" scan, which
/// (unlike this pass) had no way to distinguish a genuine stale source-space address from an
/// ordinary string byte or integer that merely happened to fall in the same numeric range. This
/// pass does not have that ambiguity: `AddressRelocations::translate` only ever rewrites a value
/// that is an *exact* match for a captured source-range address (see its doc comment) -- it cannot
/// misfire on a string byte or small integer the way pattern-matching byte VALUES can, regardless
/// of which range (stack, heap, or ELF data) is being scanned. The heap being included is
/// therefore governed by the same structural, range-membership logic as an ELF's `.data`/`.bss`:
/// private, writable, non-executable, non-stack. Excluding the heap here was tried and found to
/// be a genuine regression, not a fix: it reopens the exact `STATUS_PRIVILEGED_INSTRUCTION` crash
/// this whole investigation started from (a stale post-`fork()` pointer in mallocng's own
/// heap-resident bookkeeping -- e.g. busybox `ash`'s file-stack head -- reaching `free()`
/// untranslated and tripping mallocng's deliberate alignment `hlt`). Live-verified: with the heap
/// excluded, `sh -c "ls /; ls /usr; ls /tmp; ls /bin | head -3"` crashed 20/20 (and a shorter
/// 2-command variant crashed 3/3); with the heap included (this revision), the same repro was
/// 0/20 clean, and the `busybox echo --version`-after-churn argv-corruption repro that originally
/// motivated excluding the heap was independently confirmed clean at 60/60 with the heap
/// INCLUDED, across two different builds -- meaning the heap was never actually the source of that
/// corruption; a different, contemporaneous heuristic-scan bug was, and it is gone now. See
/// `litebox::mm::linux::is_private_data_range`'s doc comment for the fuller before/after evidence.
/// # Still scans `private_data_ranges`, not `private_data_ranges_excluding_anonymous_mmap`
///
/// See `AddressRelocations::private_data_ranges_excluding_anonymous_mmap`'s doc comment in
/// `litebox::mm` for the full history of why narrowing this scan away from mallocng's own
/// anonymous-mmap arenas looks appealing (it does eliminate one crash class) and why three
/// attempts have now been reverted: the first traded that crash for a livelock in `fork_verify`'s
/// reactive healer (closed by `fork_verify::LastLoad`'s multi-hop chain tracing); the second
/// surfaced a DIFFERENT soundness gap in case (2c) itself (values healed with no proof they were
/// ever pointer-shaped, closed by requiring `fork_verify::MIN_POINTER_ALIGN`-aligned loaded
/// values); the third -- with BOTH of those fixes in place -- reproduced a genuine, deterministic,
/// unbounded single-step livelock on the pty smoke repro (`python3 -c "import pty;
/// pty.spawn(['/bin/echo','x'])"`), confirmed via `LITEBOX_VEH_TRACE=1`: `rip` cycles through the
/// exact same ~120-instruction sequence indefinitely (e.g. `...0x66be40..0x66be95, 0x4c4c0a,
/// 0x66beb6.., 0x59495c..0x5947f4, 0x594937.., 0x66be40...` repeating byte-for-byte), never
/// terminating even after 2+ minutes, unlike the broad sweep (which lets the same repro finish in
/// well under a second). The multi-hop chain and alignment gate are each independently real,
/// sound, verified improvements to case (2c) -- but neither, together or alone, closes the
/// multi-indirection gap that makes this specific proactive-sweep coverage load-bearing: the
/// broad sweep is proactively fixing up a base pointer this loop reloads from, reached through
/// more indirection (or a different traversal shape) than one memory-load chain can trace, and
/// removing that proactive coverage leaves the reactive healer unable to ever converge on it.
/// Narrowing this scan remains NOT safe to land; left for a future pass with a genuinely deeper
/// multi-indirection reactive trace (or a different, still-narrower proactive strategy that keeps
/// covering whatever this loop's base pointer needs).
#[cfg(target_arch = "x86_64")]
fn fixup_stale_elf_data_pointers<Platform: ShimPlatform>(
    relocations: &litebox::mm::AddressRelocations,
) {
    for (source_range, dest_base) in relocations.private_data_ranges() {
        let mut addr = dest_base;
        let dest_top = dest_base + source_range.len();
        while addr < dest_top {
            let slot = UserPtrMut::<usize>::from_usize(addr);
            if let Some(value) = slot.read_at_offset::<Platform>(0)
                && let Some(translated) = relocations.translate(value)
            {
                let _ = slot.write_at_offset::<Platform>(0, translated);
            }
            addr += core::mem::size_of::<usize>();
        }
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Called when the task is exiting.
    ///
    /// # Reparenting orphaned children onto this process's own (still-live) parent, or the
    /// # bootstrap process
    ///
    /// If this call detaches this process's *last* thread (i.e. the whole process is exiting, not
    /// just one thread of a multi-threaded process), any of this process's own children that are
    /// still running and have not yet been `wait4()`-ed for are moved onto this process's own
    /// still-live parent's child list -- or, if that parent is itself already fully gone, onto the
    /// shim's bootstrap process's child list -- mirroring real Linux's reparent-to-the-nearest-
    /// live-ancestor (traditionally `init`, or a `PR_SET_CHILD_SUBREAPER`) behavior for orphaned
    /// processes.
    ///
    /// Without this, an orphaned grandchild becomes permanently unreapable: nothing in the shim
    /// ever tracked a process's grandchildren, only its immediate children (`Process::children`),
    /// so once the immediate parent exits, the grandchild's `(pid, Process)` entry existed nowhere
    /// any living task could `wait4()` for -- its eventual `exit_group()` / `detach_thread()`
    /// would still correctly decrement its own `nr_threads` and call `wake_all()`, but into a
    /// void, since no thread was ever going to be blocked in `wait_for_exit()` on it. This is not
    /// a lost-wakeup race (the wake genuinely has no listener, ever) -- it is a missing
    /// notification *path*, one layer up from the actual exit bookkeeping.
    ///
    /// This is a real, independently confirmed gap (reproduced directly, no network involved:
    /// `sh -c "timeout 5 tar -tzf <a 2-gzip-member .tar.gz>"` -- busybox's `timeout` applet forks
    /// an intermediate helper process that execs and exits almost immediately, orphaning the
    /// actual `tar` process it started; before this fix, `tar`'s successful `exit_group` had no
    /// path back to anything still waiting on it), and this fix does make that orphan reapable
    /// where it previously was not (`Process::children`/`live_parent` now correctly track and
    /// surface it). It has the same *shape* as the fork/exit chain in the deterministic
    /// `apk add nodejs` / `icu-data-en` post-install-script hang this investigation was chasing
    /// (`ash` forks a subshell to run the trigger script, the subshell forks and execs the actual
    /// script interpreter and exits before it finishes) -- but reproducing the minimal `timeout`
    /// case above with this fix applied still hangs identically, so this fix alone does *not*
    /// fully resolve that hang; there is at least one more distinct bug in that path not yet
    /// root-caused. Landed as its own correct, independently-verified fix rather than withheld
    /// pending the remainder, per this investigation's standing discipline of not overclaiming
    /// resolution.
    pub(crate) fn prepare_for_exit(&mut self) {
        litebox_util_log::debug!(tid:% = self.tid; "prepare_for_exit: entry (Task dropping)");
        // Deferred: do NOT wake `wait4`/`wait_for_exit` waiters yet. See
        // `Process::detach_thread`'s doc comment -- a parent's `wait4()` must never be allowed to
        // return before this process's fds are released below, mirroring real Linux's `do_exit()`
        // ordering (`exit_files` before the task becomes reapable). Waking early was a genuine,
        // shim-only observable-timing divergence from real Linux on this exact path: a pipeline
        // parent's `wait4()` could return while a just-exited child's pipe fd was still open from
        // a peer's point of view.
        let (notify, process_exited) = self.thread.detach_from_process_deferred();
        litebox_util_log::debug!(tid:% = self.tid, process_exited:% = process_exited; "prepare_for_exit: detach_from_process done");
        if process_exited {
            // Real Linux implicitly closes every fd a process holds when its last thread exits,
            // releasing each open file description's reference so peers (e.g. a pipe's reader,
            // waiting for EOF once the last writer goes away) are correctly notified. This
            // shim's fd bookkeeping does not do that automatically -- see
            // `close_all_fds_on_process_exit`'s doc comment for the real, reproduced hang this
            // fixes (a pipe reader blocking forever because a writer's fd was never released on
            // the writer's ordinary process exit). Must happen before reparenting orphans below,
            // though the ordering relative to reparenting is not itself load-bearing -- fd
            // closure and child reparenting are independent cleanup steps. It MUST, however,
            // happen before `notify_detached` below -- see this function's comment above.
            self.close_all_fds_on_process_exit();
            let orphans = self.process().take_children();
            if !orphans.is_empty() {
                let target = self
                    .process()
                    .live_parent()
                    .or_else(|| self.global.bootstrap_process.get().cloned());
                if let Some(target) = target
                    && !Arc::ptr_eq(&target, self.process())
                {
                    target.adopt_children(orphans);
                }
            }
        }
        if notify {
            self.process().notify_detached();
        }

        if let Some(clear_child_tid) = self.thread.clear_child_tid.take() {
            // Clear the child TID if requested
            // TODO: if we are the last thread, we don't need to clear it
            let _ = clear_child_tid.write_at_offset::<Platform>(0, 0);
            // Cast from *i32 to *u32
            let clear_child_tid = UserPtrMut::from_usize(clear_child_tid.as_usize());
            let _ = self.sys_futex(litebox_common_linux::FutexArgs::Wake {
                addr: clear_child_tid,
                flags: litebox_common_linux::FutexFlags::PRIVATE,
                count: 1,
            });
        }
        if let Some(robust_list) = self.thread.robust_list.take() {
            let _ = self.wake_robust_list(robust_list);
        }
    }

    pub(crate) fn sys_exit(&self, status: i32) {
        // The `Task` will be dropped on the way out of the shim, which will
        // call `self.prepare_for_exit()`.
        self.global.platform.end_fork_child_verification();
        self.exit_thread(status.trunc());
    }

    pub(crate) fn sys_exit_group(&self, status: i32) {
        // Tear down occurs similarly to `sys_exit`.
        self.global.platform.end_fork_child_verification();
        self.exit_group(ExitStatus::Exit(status.trunc()));
    }

    /// Handle syscall `wait4`.
    ///
    /// Supports waiting for a specific child (`pid > 0`) or any child (`pid == -1`), either
    /// blocking until the child exits or, with `WNOHANG` set, returning `0` immediately if no
    /// child has exited yet (no `WUNTRACED`/`WCONTINUED` support yet -- only `WNOHANG` is
    /// recognized in `options`). `rusage` is accepted but never populated.
    pub(crate) fn sys_wait4(
        &self,
        pid: i32,
        wstatus: Option<UserPtrMut<i32>>,
        options: i32,
        _rusage: Option<UserPtrMut<u8>>,
    ) -> Result<usize, Errno> {
        const WNOHANG: i32 = 0x1;
        let no_hang = options & WNOHANG != 0;
        let process = self.process();

        if !(pid > 0 || pid == -1) {
            // Waiting for a specific process group (pid == 0 or pid < -1) is not
            // supported yet -- every child we create is in its own group today anyway.
            log_unsupported!("wait4 with pid={pid} (process-group wait)");
            return Err(Errno::EINVAL);
        }

        // Cross-process children (pass 141, `LITEBOX_PROCESS_FORK=1`) are tracked in a separate
        // registry from thread-based children (see `Process::cross_process_children`'s doc
        // comment on why: a genuinely separate OS process cannot share this process's
        // `Arc<Process>`). Check it FIRST for a targeted `pid > 0` wait -- a pid can only ever
        // appear in one of the two registries -- and prefer it over `children` for `pid == -1`
        // only when `children` itself has nothing to offer, so an existing thread-based-only
        // caller's `pid == -1` behavior is completely unaffected by this addition.
        if pid > 0
            && let Some(handle) = process.find_cross_process_child(pid)
        {
            let raw_exit = if no_hang {
                let Some(raw_exit) = self.global.platform.try_wait_for_cross_process_exit(handle)
                else {
                    return Ok(0);
                };
                raw_exit
            } else {
                self.global.platform.wait_for_cross_process_exit(handle)
            };
            process.reap_cross_process_child(pid);
            let encoded = decode_cross_process_wait_status(raw_exit);
            if let Some(wstatus) = wstatus {
                let _ = wstatus.write_at_offset::<Platform>(0, encoded);
            }
            return Ok(usize::try_from(pid).unwrap());
        }

        // Unlike the blocking path, a `WNOHANG` poll must NOT remove the child from our children
        // list unless it has actually already exited -- otherwise a later real wait for that
        // same child would incorrectly see `ECHILD`.
        let (child_pid, child_process) = {
            let children = process.children.lock();
            let idx = if pid > 0 {
                children.iter().position(|(p, _)| *p == pid)
            } else {
                children.first().map(|_| 0)
            };
            let Some(idx) = idx else {
                return Err(Errno::ECHILD);
            };
            let (child_pid, child_process) = &children[idx];
            (*child_pid, child_process.clone())
        };

        let exit_status = if no_hang {
            let Some(exit_status) = child_process.try_wait_for_exit() else {
                return Ok(0);
            };
            exit_status
        } else {
            child_process.wait_for_exit()
        };

        // The child has exited (or we were willing to block until it did) -- now it's safe to
        // remove it from our children list. Linux does not let you wait for the same child
        // twice, so this must happen exactly once, after confirming exit.
        process.children.lock().retain(|(p, _)| *p != child_pid);

        let encoded = match exit_status {
            // Linux wait status encoding: normal exit is (exit_code & 0xff) << 8.
            ExitStatus::Exit(code) => (i32::from(code) & 0xff) << 8,
            // Death by signal: the low 7 bits hold the signal number (bit 7 set = core dumped,
            // never set here).
            ExitStatus::Signal(sig) => sig.as_i32() & 0x7f,
        };
        if let Some(wstatus) = wstatus {
            let _ = wstatus.write_at_offset::<Platform>(0, encoded);
        }

        Ok(usize::try_from(child_pid).unwrap())
    }
}

/// A descriptor for thread-local storage (TLS).
///
/// On `x86_64`, this is represented as a `*mut u8`. The TLS pointer can point to
/// an arbitrary-sized memory region.
#[cfg(target_arch = "x86_64")]
type ThreadLocalDescriptor = UserPtrMut<u8>;

struct NewThreadArgs<Platform: ShimPlatform, FS: ShimFS> {
    /// Task struct that maintains all per-thread data
    task: Task<Platform, FS>,
}

impl<Platform: ShimPlatform, FS: ShimFS> litebox::shim::InitThread for NewThreadArgs<Platform, FS> {
    type ExecutionContext = litebox_common_linux::PtRegs;

    fn init(
        self: alloc::boxed::Box<Self>,
    ) -> alloc::boxed::Box<dyn litebox::shim::EnterShim<ExecutionContext = Self::ExecutionContext>>
    {
        let Self { task } = *self;

        Box::new(crate::LinuxShimEntrypoints {
            task,
            _not_send: core::marker::PhantomData,
        })
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    pub(crate) fn sys_clone(
        &self,
        ctx: &litebox_common_linux::PtRegs,
        args: &litebox_common_linux::CloneArgs,
    ) -> Result<usize, Errno> {
        self.do_clone(ctx, args, false)
    }

    pub(crate) fn sys_clone3(
        &self,
        ctx: &litebox_common_linux::PtRegs,
        args: UserPtr<litebox_common_linux::CloneArgs>,
    ) -> Result<usize, Errno> {
        let args = args.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        self.do_clone(ctx, &args, true)
    }

    /// Creates a new thread or process.
    ///
    /// Thread-style clone requires VM, THREAD, SIGHAND, and FILES all set (sharing address
    /// space, thread group, signal handlers, and fd table with the caller). Process-style clone
    /// (real `fork()`/`vfork()`) requires NONE of VM/THREAD/SIGHAND/FILES set: the child gets
    /// its own address space (an eager duplicate of the caller's, at possibly-different host
    /// addresses -- see [`litebox::mm::PageManager::duplicate`]'s doc comment on the resulting
    /// address-relocation limitation), its own thread group, and its own fd table (an
    /// independent copy sharing the same underlying open file descriptions).
    #[expect(
        clippy::similar_names,
        reason = "pid/ppid is standard Unix terminology"
    )]
    fn do_clone(
        &self,
        ctx: &litebox_common_linux::PtRegs,
        args: &litebox_common_linux::CloneArgs,
        clone3: bool,
    ) -> Result<usize, Errno> {
        const MAX_SIGNAL_NUMBER: u64 = 64;

        let litebox_common_linux::CloneArgs {
            mut flags,
            pidfd: _,
            child_tid,
            parent_tid,
            exit_signal,
            stack,
            stack_size,
            tls,
            set_tid,
            set_tid_size,
            cgroup,
        } = *args;

        // `CLONE_DETACHED` is ignored but has been reserved for reuse with
        // `clone3` or in combination with `CLONE_PIDFD`.
        if !clone3 && !flags.contains(CloneFlags::PIDFD) {
            flags.remove(CloneFlags::DETACHED);
        }

        let thread_clone_flags =
            CloneFlags::VM | CloneFlags::THREAD | CloneFlags::SIGHAND | CloneFlags::FILES;

        // Real `fork()`/`vfork()` set none of VM/THREAD/SIGHAND/FILES: the child gets its own
        // address space, its own thread group (i.e. becomes a new process), its own signal
        // handler table copy, and its own (initially fd-table-copied) files. `vfork()` sets
        // VFORK in addition; it is otherwise the same shape (see sys_vfork's caller, which sets
        // exit_signal but not VM/THREAD/SIGHAND/FILES either).
        let is_process_clone = !flags.intersects(
            CloneFlags::VM | CloneFlags::THREAD | CloneFlags::SIGHAND | CloneFlags::FILES,
        );

        let supported_clone_flags = CloneFlags::VM
            | CloneFlags::FS
            | CloneFlags::FILES
            | CloneFlags::SIGHAND
            | CloneFlags::PARENT
            | CloneFlags::THREAD
            | CloneFlags::SETTLS
            | CloneFlags::PARENT_SETTID
            | CloneFlags::CHILD_CLEARTID
            | CloneFlags::CHILD_SETTID
            | CloneFlags::VFORK
            // Ignored since we don't support sysv semaphores anyway.
            | CloneFlags::SYSVSEM;

        if flags.intersects(!supported_clone_flags) {
            log_unsupported!(
                "clone with unsupported flags: {:?}",
                flags & !supported_clone_flags
            );
            return Err(Errno::EINVAL);
        }
        if !is_process_clone && !flags.contains(thread_clone_flags) {
            log_unsupported!(
                "clone with missing required flags: {:?}",
                thread_clone_flags & !flags
            );
            return Err(Errno::EINVAL);
        }

        if cgroup != 0 {
            log_unsupported!("clone with cgroup");
            return Err(Errno::EINVAL);
        }

        if set_tid != 0 || set_tid_size != 0 {
            log_unsupported!("clone with set_tid");
            return Err(Errno::EINVAL);
        }

        // TODO: `exit_signal` is validated but not yet delivered to the parent on child exit
        // (no SIGCHLD support yet -- see sys_wait4/waitpid, also not yet implemented).
        if exit_signal > MAX_SIGNAL_NUMBER {
            return Err(Errno::EINVAL);
        }

        let tls = if flags.contains(CloneFlags::SETTLS) {
            let addr = tls.trunc();
            #[cfg(target_arch = "x86_64")]
            {
                // Validate the user-controlled TLS base before spawning the thread. Two checks,
                // deliberately layered: `is_valid_user_fs_base` enforces the generic x86_64 Linux
                // ABI ceiling (`USER_ADDR_END`, the same on every platform this shim targets), but
                // that ceiling is NOT tight enough on `litebox_platform_windows_userland`
                // specifically, whose actual guest-addressable ceiling (`TASK_ADDR_MAX`) sits far
                // below `USER_ADDR_END` -- everything from `TASK_ADDR_MAX` up to
                // `HOST_ALLOCATOR_REGION_MIN`'s reserved 64 GiB span belongs to the HOST process
                // (its own stack, modules, and the host global allocator's own reserved region;
                // see `HOST_ALLOCATOR_REGION_MIN`'s doc comment), never to the guest. A guest-
                // supplied (or, more concerningly, a corrupted/mistranslated) TLS base landing up
                // there would pass the generic ABI check yet still let the guest's own `%fs`-
                // relative TLS/TCB accesses dereference live host heap memory instead of guest
                // memory -- exactly the shape of this investigation's long-standing musl dtv-clear
                // crash (`mov rdx, [rax+0x80]` on a `rax` proven to be a live `HOST_ALLOCATOR_
                // REGION_MIN`-range address). Rejecting it here closes that off at the one syscall
                // that can set a thread's FS base directly, regardless of how such a value could
                // ever have been computed.
                if !litebox_common_linux::arch::is_valid_user_fs_base(addr)
                    || addr >= Platform::TASK_ADDR_MAX
                {
                    return Err(Errno::EPERM);
                }
            }
            #[cfg(target_arch = "x86_64")]
            let desc = UserPtrMut::from_usize(addr);
            Some(desc)
        } else {
            None
        };

        let child_tid = if child_tid == 0 {
            None
        } else {
            Some(UserPtrMut::from_usize(child_tid.trunc()))
        };
        let set_child_tid = if flags.contains(CloneFlags::CHILD_SETTID) {
            child_tid
        } else {
            None
        };
        let clear_child_tid = if flags.contains(CloneFlags::CHILD_CLEARTID) {
            child_tid
        } else {
            None
        };
        let set_parent_tid = if flags.contains(CloneFlags::PARENT_SETTID) && parent_tid != 0 {
            Some(UserPtrMut::from_usize(parent_tid.trunc()))
        } else {
            None
        };

        let fs = if flags.contains(CloneFlags::FS) {
            self.fs.borrow().clone()
        } else {
            alloc::sync::Arc::new((**self.fs.borrow()).clone())
        };
        let files = if flags.contains(CloneFlags::FILES) {
            self.files.borrow().clone()
        } else {
            alloc::sync::Arc::new(self.files.borrow().fork_duplicate(&self.global.litebox))
        };

        let child_tid = self.global.next_thread_id.fetch_add(1, Ordering::Relaxed);
        if let Some(parent_tid_ptr) = set_parent_tid {
            let _ = parent_tid_ptr.write_at_offset::<Platform>(0, child_tid);
        }

        if (stack == 0 && stack_size != 0) || (stack != 0 && clone3 && stack_size == 0) {
            return Err(Errno::EINVAL);
        }
        let sp = if stack != 0 {
            let stack: usize = stack.trunc();
            Some(stack.wrapping_add(stack_size.trunc()))
        } else {
            None
        };

        let (thread, init_state, pid, ppid, child_shared_pending) = if is_process_clone {
            // Real `fork()`/`vfork()`: build a brand-new `Process` (new thread group) whose
            // address space is an eager duplicate of the parent's -- writes made by either the
            // parent or the child after this point are independent.
            let (dest_pm, relocations) =
                unsafe { self.process().pm.duplicate(&self.global.litebox) }.map_err(|err| {
                    litebox_util_log::error!(err:% = err; "failed to duplicate address space for fork()");
                    Errno::ENOMEM
                })?;
            // Diagnostic-only (pass 111, `LITEBOX_DIAG_PROCESS_FORK_SPAWN=1`, off by default): a
            // no-op on every platform except `litebox_platform_windows_userland`, and a no-op
            // there too unless the env var is set. Runs on the PARENT's own thread, right after
            // the real duplication this actual `fork()` call just performed, and does not affect
            // this `fork()` call's real outcome -- see `ForkChildVerificationProvider::
            // diagnostic_process_fork_probe`'s doc comment for the full mechanism and safety
            // argument.
            //
            // `fd_complexity` (pass 116) is a cheap, read-only scan of the pre-fork fd table --
            // `RawDescriptorStorage::iter_alive` over `self.files`, the SAME table
            // `fork_duplicate` (below) is about to duplicate -- computed unconditionally (it is
            // O(occupied fd count) and touches no subsystem state) so the diagnostic hook can
            // classify this fork() call's fd complexity without a second, gated fd-table walk.
            // See `litebox::platform::ForkFdComplexity`'s doc comment for why this matters.
            let fd_complexity = {
                let files = self.files.borrow();
                let raw_descriptors = files.raw_descriptor_store.read();
                let total_alive = raw_descriptors.iter_alive().count();
                let beyond_stdio = raw_descriptors.iter_alive().filter(|&raw| raw >= 3).count();
                litebox::platform::ForkFdComplexity {
                    total_alive,
                    beyond_stdio,
                }
            };
            let vforked = flags.contains(CloneFlags::VFORK);
            // Created once here and threaded into both the new `Process` (below) and the new
            // `Task`'s `SignalState` (see `clone_for_new_task`'s call site further down) -- they
            // must end up sharing the exact same `Arc`, not two independently allocated queues.
            let child_shared_pending = Arc::new(Mutex::new(super::signal::PendingSignals::new()));
            let thread = crate::syscalls::process::ThreadState::new_process(
                child_tid,
                dest_pm,
                vforked,
                Some(Arc::downgrade(self.process())),
                child_shared_pending.clone(),
            );
            // Real fork() inherits the parent's current rlimits rather than resetting to
            // program-start defaults (see `ResourceLimits::copy_from`'s doc comment).
            thread.process.limits.copy_from(&self.process().limits);

            // The captured ctx's registers may hold addresses into the PARENT's address space --
            // the child's code, stack, and everything else generally live at a different host
            // address after `Vmem::duplicate` (see its doc comment). rip/rsp/rbp obviously need
            // translation or the child crashes immediately; but the x86_64 SysV ABI also
            // guarantees callee-saved registers (rbx, r12-r15) survive a `syscall` unchanged, so
            // guest code routinely keeps a live pointer in one of them across `clone()` too --
            // and the caller-clobbered registers can just as well hold a reloaded pointer value
            // on return. Translate every register uniformly; `AddressRelocations::translate`
            // already safely returns `None` (leaving the value untouched) for anything that
            // isn't a relocated address, so this is a no-op for non-pointer register contents
            // (small integers, flags, the post-fork `rax=0` return value, etc.).
            let mut child_ctx = ctx.clone();
            #[cfg(target_arch = "x86_64")]
            {
                macro_rules! translate_reg {
                    ($reg:ident) => {
                        if let Some(new_val) = relocations.translate(child_ctx.$reg) {
                            child_ctx.$reg = new_val;
                        }
                    };
                }
                translate_reg!(rip);
                translate_reg!(rsp);
                translate_reg!(rbp);
                translate_reg!(rbx);
                translate_reg!(r12);
                translate_reg!(r13);
                translate_reg!(r14);
                translate_reg!(r15);
                translate_reg!(rax);
                translate_reg!(rcx);
                translate_reg!(rdx);
                translate_reg!(rsi);
                translate_reg!(rdi);
                translate_reg!(r8);
                translate_reg!(r9);
                translate_reg!(r10);
                translate_reg!(r11);
                // Clear privileged/reserved RFLAGS bits and normalize CS/SS to the user ABI
                // values before this context is ever resumed on a brand-new thread -- see
                // PtRegs::sanitize_for_user_return's doc comment.
                let sanitized = child_ctx.sanitize_for_user_return();
                debug_assert!(
                    sanitized,
                    "forked child's rip/rsp left the user address range"
                );

                // Register translation above covers every GPR at the instant `fork()` returns,
                // but the very next thing the child does -- before it ever reaches `execve()` --
                // is unwind back up through libc's own `fork()`/`clone()` call chain (musl's
                // `_Fork` -> `fork` -> the guest's caller, e.g. ash's `forkshell`), which
                // necessarily `ret`s to, and reloads spilled registers from, a handful of stack
                // and TCB slots that were written by `call` instructions (or cached by musl's TLS
                // setup) BEFORE this `fork()` happened. Those slots hold verbatim-copied,
                // untranslated pointers into the PARENT's address space -- the same "stale
                // pointer" class documented on `fork_verify`, just fixed up proactively here
                // instead of only reactively during single-step verification (which still covers
                // whatever this proactive pass does not reach, e.g. a value copied into a
                // register from here and then re-spilled by the child's own early code). Unlike
                // an unbounded scan of arbitrary heap/global memory (tried and reverted earlier in
                // this investigation -- see `fork_verify`'s module docs), this is a small,
                // deterministic, always-present set of slots confined to `duplicate()`'s own
                // duplicated regions: fix them up here, once, before the child ever executes an
                // instruction, exactly the same way the FS-base self-pointer below is fixed up
                // for the same reason.
                fixup_stale_stack_pointers::<Platform>(&relocations, child_ctx.rsp);
                fixup_stale_elf_data_pointers::<Platform>(&relocations);
            }

            // Diagnostic-only (pass 111, `LITEBOX_DIAG_PROCESS_FORK_SPAWN=1`, off by default): a
            // no-op on every platform except `litebox_platform_windows_userland`, and a no-op
            // there too unless the env var is set. Runs on the PARENT's own thread, right after
            // `child_ctx`'s registers have been translated above, and does not affect this
            // `fork()` call's real outcome -- see `ForkChildVerificationProvider::
            // diagnostic_process_fork_probe`'s doc comment for the full mechanism and safety
            // argument. Called here (rather than immediately after `pm.duplicate` as before pass
            // 118) specifically so `translated_gprs` can carry the already-translated rip/rsp/rax
            // pass 118's `SetThreadContext` injection probe needs -- computing it required moving
            // this call site past the register-translation block above.
            #[cfg(target_arch = "x86_64")]
            let translated_gprs = Some(litebox::platform::ForkGprSnapshot {
                rip: child_ctx.rip,
                rsp: child_ctx.rsp,
                rax: child_ctx.rax,
            });
            // Pass 120: the SAME already-translated `child_ctx`, carried in full (every GPR plus
            // eflags/cs/ss) alongside the pre-existing 3-field snapshot -- see
            // `ForkFullGprSnapshot`'s doc comment for why this is a second, wider struct rather
            // than widening `ForkGprSnapshot` in place.
            #[cfg(target_arch = "x86_64")]
            let full_translated_gprs = Some(litebox::platform::ForkFullGprSnapshot {
                r15: child_ctx.r15,
                r14: child_ctx.r14,
                r13: child_ctx.r13,
                r12: child_ctx.r12,
                rbp: child_ctx.rbp,
                rbx: child_ctx.rbx,
                r11: child_ctx.r11,
                r10: child_ctx.r10,
                r9: child_ctx.r9,
                r8: child_ctx.r8,
                rax: child_ctx.rax,
                rcx: child_ctx.rcx,
                rdx: child_ctx.rdx,
                rsi: child_ctx.rsi,
                rdi: child_ctx.rdi,
                orig_rax: child_ctx.orig_rax,
                rip: child_ctx.rip,
                cs: child_ctx.cs,
                eflags: child_ctx.eflags,
                rsp: child_ctx.rsp,
                ss: child_ctx.ss,
            });
            #[cfg(not(target_arch = "x86_64"))]
            let full_translated_gprs = None;
            self.global.platform.diagnostic_process_fork_probe(
                &relocations,
                fd_complexity,
                translated_gprs,
                full_translated_gprs,
            );

            // Pass 141 diagnostic-only proof (`LITEBOX_DIAG_PROCESS_FORK_WAIT4=1`, off by
            // default): live-exercises the cross-process wait4() bridge against a real Windows
            // child, registered into THIS process's real `cross_process_children` registry, then
            // immediately reaped via a real `sys_wait4` call for the diagnostic pid -- proving
            // the registry/HANDLE-wait/exit-code-decode path end-to-end without touching this
            // actual fork() call's real (thread-based) outcome. See
            // `ForkChildVerificationProvider::diagnostic_cross_process_wait4_probe`'s doc
            // comment.
            {
                let process = self.process().clone();
                let mut register =
                    |diag_pid: i32, handle: litebox::platform::CrossProcessChildHandle| {
                        process.register_cross_process_child(diag_pid, handle);
                    };
                self.global
                    .platform
                    .diagnostic_cross_process_wait4_probe(&mut register);
            }

            // Register the new process as a child of the caller's process so a later
            // `wait4`/`waitpid` from the parent can find it.
            self.process()
                .children
                .lock()
                .push((child_tid, thread.process.clone()));

            // The child runs on a brand-new host thread, whose platform-level FS base (backing
            // the guest's TLS pointer) starts unset -- explicitly propagate the parent's current
            // value so the child's TLS accesses (which libc issues immediately after `clone()`
            // returns) don't dereference FS base 0. See `ThreadInitState::ForkedChild`'s doc
            // comment.
            //
            // Like `rip`/`rsp`/`rbp` above, this is a guest address (validated against
            // `USER_ADDR_END` by `is_valid_user_fs_base`), not a host pointer -- and it points at
            // musl's `struct pthread` TCB, which per musl's own `__init_tls.c` layout lives
            // directly adjacent to the thread's stack, i.e. within the same relocation group
            // `Vmem::duplicate` may move for the child. Without translation the child's `%fs`
            // would point at the PARENT's original TCB address instead of its own relocated
            // copy, and the child's first TLS access (which musl issues immediately on return
            // from `clone()`) would dereference the wrong guest address.
            #[cfg(target_arch = "x86_64")]
            let fs_base = {
                let parent_fs_base = self
                    .global
                    .platform
                    .get_arch_specific_register(&ArchSpecificRegister::FsBase)
                    .map_err(Errno::from)?;
                let child_fs_base = relocations
                    .translate(parent_fs_base)
                    .unwrap_or(parent_fs_base);

                // The x86-64 TLS ABI requires the thread pointer to be *self-referential*: the
                // word at `%fs:0` holds the thread pointer's own value, and that is how position-
                // independent code materializes it at all (`mov reg, fs:[0]`, which musl's
                // `__pthread_self()` -- and hence every `errno` access -- compiles to; the CPU
                // cannot read `%fs.base` directly from user mode). That word lives in *memory*,
                // so `Vmem::duplicate` copies it verbatim and it still holds the PARENT's TCB
                // address in the child. Translating the FS base register alone therefore is not
                // enough: the child's very first `errno` write would compute its address from
                // the stale self-pointer and land in the parent's live TCB.
                //
                // Fix up that one ABI-mandated slot so the child's thread pointer is
                // self-consistent, exactly as the register translation above intends.
                if child_fs_base != parent_fs_base {
                    let slot = UserPtrMut::<usize>::from_usize(child_fs_base);
                    let _ = slot.write_at_offset::<Platform>(0, child_fs_base);
                }

                child_fs_base
            };
            #[cfg(not(target_arch = "x86_64"))]
            let fs_base = 0;

            (
                thread,
                ThreadInitState::ForkedChild(
                    child_ctx,
                    fs_base,
                    alloc::sync::Arc::new(relocations),
                ),
                child_tid,
                self.pid,
                Some(child_shared_pending),
            )
        } else {
            let thread = self.thread.new_thread(child_tid).ok_or(Errno::EBUSY)?;
            (
                thread,
                ThreadInitState::NewThread {
                    stack: sp,
                    tls,
                    set_child_tid,
                },
                self.pid,
                self.ppid,
                None,
            )
        };
        thread.init_state.set(init_state);
        thread.clear_child_tid.set(clear_child_tid);

        // Captured before `thread` moves into the spawned `Task` below -- only actually blocked
        // on when this is a `vfork()`'d process (see `Process::wait_for_vfork_done`'s doc
        // comment); `None` for a thread-clone or a plain `fork()`.
        let vfork_child_process =
            (is_process_clone && flags.contains(CloneFlags::VFORK)).then(|| thread.process.clone());

        let r = unsafe {
            self.global.platform.spawn_thread(
                ctx,
                Box::new(NewThreadArgs {
                    task: Task {
                        global: self.global.clone(),
                        wait_state: crate::wait::WaitState::new(self.global.platform),
                        thread,
                        pid,
                        tid: child_tid,
                        ppid,
                        credentials: self.credentials.clone(),
                        comm: self.comm.clone(),
                        fs: fs.into(),
                        files: files.into(),
                        signals: self.signals.clone_for_new_task(child_shared_pending),
                    },
                }),
            )
        };
        if let Err(err) = r {
            litebox_util_log::error!(err:% = err; "failed to spawn thread");
            // Treat all spawn errors as `ENOMEM`. `EAGAIN` and other errors are
            // for conditions the user can control (such as "in-shim" rlimit
            // violations).
            return Err(Errno::ENOMEM);
        }
        litebox_util_log::debug!(
            parent_tid:% = self.tid,
            child_tid:% = child_tid,
            flags:? = flags,
            is_process_clone:% = is_process_clone;
            "clone: spawned new task"
        );

        // `vfork()`'s POSIX contract: the calling thread is suspended until the child calls
        // `execve` or exits. Block AFTER the child's host thread has been successfully spawned
        // (so the child is guaranteed to make progress -- there is no deadlock risk here, unlike
        // blocking while still holding any lock the child might need).
        if let Some(vfork_child_process) = vfork_child_process {
            vfork_child_process.wait_for_vfork_done();
        }

        Ok(usize::try_from(child_tid).unwrap())
    }

    /// Handle syscall `set_tid_address`.
    pub(crate) fn sys_set_tid_address(&self, tidptr: UserPtrMut<i32>) -> i32 {
        self.thread.clear_child_tid.set(Some(tidptr));
        self.tid
    }

    /// Handle syscall `gettid`.
    pub(crate) fn sys_gettid(&self) -> i32 {
        self.tid
    }
}

// TODO: enforce the following limits:
pub(crate) const RLIMIT_NOFILE_CUR: usize = 1024 * 1024;
const RLIMIT_NOFILE_MAX: usize = 1024 * 1024;

/// Default `RLIMIT_SIGPENDING` cur/max, matching a typical unprivileged Linux
/// process (`ulimit -i`). Unlike most other resources this one is actually
/// enforced (see `SignalQueue::push`), so it must not default to 0 -- a zero
/// limit would silently drop every real-time/queued signal a guest sends.
const RLIMIT_SIGPENDING_DEFAULT: usize = 62719;

struct AtomicRlimit {
    cur: core::sync::atomic::AtomicUsize,
    max: core::sync::atomic::AtomicUsize,
}

impl AtomicRlimit {
    const fn new(cur: usize, max: usize) -> Self {
        Self {
            cur: core::sync::atomic::AtomicUsize::new(cur),
            max: core::sync::atomic::AtomicUsize::new(max),
        }
    }
}

pub(crate) struct ResourceLimits {
    limits: [AtomicRlimit; litebox_common_linux::RlimitResource::RLIM_NLIMITS],
}

impl ResourceLimits {
    const fn default() -> Self {
        // Every resource defaults to "unlimited" (matching what an unprivileged
        // process typically sees for the resources LiteBox doesn't actually
        // enforce), except the handful below that LiteBox tracks for real.
        seq_macro::seq!(N in 0..16 {
            let mut limits = [
                #(
                    AtomicRlimit::new(
                        litebox_common_linux::rlim_t::MAX,
                        litebox_common_linux::rlim_t::MAX,
                    ),
                )*
            ];
        });
        limits[litebox_common_linux::RlimitResource::NOFILE as usize] = AtomicRlimit {
            cur: core::sync::atomic::AtomicUsize::new(RLIMIT_NOFILE_CUR),
            max: core::sync::atomic::AtomicUsize::new(RLIMIT_NOFILE_MAX),
        };
        limits[litebox_common_linux::RlimitResource::STACK as usize] = AtomicRlimit {
            cur: core::sync::atomic::AtomicUsize::new(crate::loader::DEFAULT_STACK_SIZE),
            max: core::sync::atomic::AtomicUsize::new(litebox_common_linux::rlim_t::MAX),
        };
        limits[litebox_common_linux::RlimitResource::SIGPENDING as usize] = AtomicRlimit {
            cur: core::sync::atomic::AtomicUsize::new(RLIMIT_SIGPENDING_DEFAULT),
            max: core::sync::atomic::AtomicUsize::new(RLIMIT_SIGPENDING_DEFAULT),
        };
        Self { limits }
    }

    /// Overwrite every limit in `self` with the corresponding value from `other`, in place --
    /// used by `fork()`/`clone()` (real process clone) so a freshly constructed child's limits
    /// (which start out as `ResourceLimits::default()`) are replaced with the *parent's current*
    /// limits rather than always resetting to program-start defaults.
    ///
    /// Real Linux inherits `rlimit`s across `fork()` (a parent that lowered e.g.
    /// `RLIMIT_NOFILE` before spawning a child expects that child to actually be bounded by it);
    /// before this, every new `Process` -- including every forked child -- kept its brand-new
    /// `ResourceLimits::default()` forever, silently discarding whatever the parent had
    /// configured.
    pub(crate) fn copy_from(&self, other: &Self) {
        for (mine, theirs) in self.limits.iter().zip(other.limits.iter()) {
            mine.cur
                .store(theirs.cur.load(Ordering::Relaxed), Ordering::Relaxed);
            mine.max
                .store(theirs.max.load(Ordering::Relaxed), Ordering::Relaxed);
        }
    }

    pub(crate) fn get_rlimit(
        &self,
        resource: litebox_common_linux::RlimitResource,
    ) -> litebox_common_linux::Rlimit {
        let r = &self.limits[resource as usize];
        litebox_common_linux::Rlimit {
            rlim_cur: r.cur.load(Ordering::Relaxed),
            rlim_max: r.max.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn get_rlimit_cur(&self, resource: litebox_common_linux::RlimitResource) -> usize {
        let r = &self.limits[resource as usize];
        r.cur.load(Ordering::Relaxed)
    }

    fn set_rlimit(
        &self,
        resource: litebox_common_linux::RlimitResource,
        new_limit: litebox_common_linux::Rlimit,
    ) {
        let r = &self.limits[resource as usize];
        r.cur.store(new_limit.rlim_cur, Ordering::Relaxed);
        r.max.store(new_limit.rlim_max, Ordering::Relaxed);
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Get resource limits, and optionally set new limits.
    pub(crate) fn do_prlimit(
        &self,
        resource: litebox_common_linux::RlimitResource,
        new_limit: Option<litebox_common_linux::Rlimit>,
    ) -> Result<litebox_common_linux::Rlimit, Errno> {
        let old_rlimit = self.thread.process.limits.get_rlimit(resource);
        if let Some(new_limit) = new_limit {
            if new_limit.rlim_cur > new_limit.rlim_max {
                return Err(Errno::EINVAL);
            }
            if let litebox_common_linux::RlimitResource::NOFILE = resource
                && new_limit.rlim_max > RLIMIT_NOFILE_MAX
            {
                return Err(Errno::EPERM);
            }
            // Note process with `CAP_SYS_RESOURCE` can increase the hard limit, but we don't
            // support capabilities in LiteBox, so we don't check for that here.
            if new_limit.rlim_max > old_rlimit.rlim_max {
                return Err(Errno::EPERM);
            }
            // Every resource accepts and remembers the new limit (so a later
            // getrlimit sees it and enforced resources like NOFILE/SIGPENDING
            // pick it up), even though most resources beyond NOFILE/STACK/
            // SIGPENDING aren't actually enforced by LiteBox -- matching this
            // build's documented "no host-enforced resource limits" boundary
            // without making ordinary `ulimit -c 0`/`ulimit -s ...` calls
            // panic the whole runner.
            let new_max_fd = new_limit.rlim_cur.saturating_sub(1);
            self.thread.process.limits.set_rlimit(resource, new_limit);
            if let litebox_common_linux::RlimitResource::NOFILE = resource {
                self.files.borrow().set_max_fd(new_max_fd);
            }
        }
        Ok(old_rlimit)
    }

    /// Handle syscall `prlimit64`.
    pub(crate) fn sys_prlimit(
        &self,
        pid: i32,
        resource: litebox_common_linux::RlimitResource,
        new_rlim: Option<UserPtr<litebox_common_linux::Rlimit64>>,
        old_rlim: Option<UserPtrMut<litebox_common_linux::Rlimit64>>,
    ) -> Result<(), Errno> {
        // `pid == 0` means "the calling process" per prlimit(2); `pid == self.pid` is exactly
        // equivalent (e.g. the util-linux `prlimit` CLI, unlike getrlimit()/setrlimit() callers,
        // defaults to passing its own real pid rather than 0). Both target self, which this shim
        // can always answer. A genuine *other* pid can't be reached: there's no shim-wide
        // process registry to look one up (see the same limitation `kill()`/`tkill()` document).
        if pid != 0 && pid != self.pid {
            log_unsupported!("prlimit64 for a remote pid");
            return Err(Errno::ESRCH);
        }
        let new_limit = match new_rlim {
            Some(rlim) => {
                let rlim = rlim.read_at_offset::<Platform>(0).ok_or(Errno::EINVAL)?;
                Some(litebox_common_linux::rlimit64_to_rlimit(rlim))
            }
            None => None,
        };
        let old_limit =
            litebox_common_linux::rlimit_to_rlimit64(self.do_prlimit(resource, new_limit)?);
        if let Some(old_rlim) = old_rlim {
            old_rlim
                .write_at_offset::<Platform>(0, old_limit)
                .ok_or(Errno::EINVAL)?;
        }
        Ok(())
    }

    /// Handle syscall `setrlimit`.
    pub(crate) fn sys_getrlimit(
        &self,
        resource: litebox_common_linux::RlimitResource,
        rlim: UserPtrMut<litebox_common_linux::Rlimit>,
    ) -> Result<(), Errno> {
        let old_limit = self.do_prlimit(resource, None)?;
        rlim.write_at_offset::<Platform>(0, old_limit)
            .ok_or(Errno::EINVAL)
    }

    /// Handle syscall `setrlimit`.
    pub(crate) fn sys_setrlimit(
        &self,
        resource: litebox_common_linux::RlimitResource,
        rlim: UserPtr<litebox_common_linux::Rlimit>,
    ) -> Result<(), Errno> {
        let new_limit = rlim.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        let _ = self.do_prlimit(resource, Some(new_limit))?;
        Ok(())
    }

    /// Handle syscall `set_robust_list`.
    pub(crate) fn sys_set_robust_list(&self, head: usize) {
        let head = UserPtr::from_usize(head);
        self.thread.robust_list.set(Some(head));
    }

    /// Handle syscall `get_robust_list`.
    pub(crate) fn sys_get_robust_list(
        &self,
        pid: Option<i32>,
        head_ptr: UserPtrMut<usize>,
    ) -> Result<(), Errno> {
        if pid.is_some() {
            unimplemented!("Getting robust list for a specific PID is not supported yet");
        }
        let head = self
            .thread
            .robust_list
            .get()
            .map_or(0, |ptr| ptr.as_usize());
        head_ptr
            .write_at_offset::<Platform>(0, head)
            .ok_or(Errno::EFAULT)
    }

    pub(crate) fn real_time_as_duration_since_epoch(&self) -> core::time::Duration {
        let now = self.global.platform.current_time();
        let unix_epoch = <Platform as TimeProvider>::SystemTime::UNIX_EPOCH;
        now.duration_since(&unix_epoch)
            .expect("must be after unix epoch")
    }

    /// Handle syscall `clock_gettime`.
    pub(crate) fn sys_clock_gettime(
        &self,
        clockid: litebox_common_linux::ClockId,
        tp: TimeParam,
    ) -> Result<(), Errno> {
        let duration = self.gettime_as_duration(clockid)?;
        tp.write::<Platform>(duration)
    }

    fn gettime_as_duration(
        &self,
        clockid: litebox_common_linux::ClockId,
    ) -> Result<core::time::Duration, Errno> {
        let duration = match clockid {
            litebox_common_linux::ClockId::RealTime => {
                // CLOCK_REALTIME
                self.real_time_as_duration_since_epoch()
            }
            litebox_common_linux::ClockId::Monotonic => {
                // CLOCK_MONOTONIC
                self.global
                    .platform
                    .now()
                    .duration_since(&self.global.boot_time)
            }
            litebox_common_linux::ClockId::MonotonicCoarse
            | litebox_common_linux::ClockId::MonotonicRaw
            | litebox_common_linux::ClockId::Boottime => {
                // CLOCK_MONOTONIC_COARSE / CLOCK_MONOTONIC_RAW / CLOCK_BOOTTIME - all
                // approximated by reusing CLOCK_MONOTONIC's source. litebox does not
                // distinguish NTP-adjustment or suspend time from plain monotonic time.
                self.global
                    .platform
                    .now()
                    .duration_since(&self.global.boot_time)
            }
            litebox_common_linux::ClockId::RealTimeCoarse => {
                // CLOCK_REALTIME_COARSE - approximated by reusing CLOCK_REALTIME's source.
                self.real_time_as_duration_since_epoch()
            }
            litebox_common_linux::ClockId::ProcessCputimeId
            | litebox_common_linux::ClockId::ThreadCputimeId => {
                // CLOCK_PROCESS_CPUTIME_ID / CLOCK_THREAD_CPUTIME_ID - litebox does not
                // track genuine per-process/per-thread CPU-time-consumed accounting.
                // Approximate with monotonic wall-clock time: callers (e.g. V8/abseil)
                // generally require a valid, monotonically-increasing, non-EINVAL value
                // for coarse profiling/scheduling decisions rather than exact CPU
                // accounting.
                self.global
                    .platform
                    .now()
                    .duration_since(&self.global.boot_time)
            }
            _ => {
                log_unsupported!("gettime for {clockid:?}");
                return Err(Errno::EINVAL);
            }
        };
        Ok(duration)
    }

    /// Convert an absolute time, specified as a duration since the epoch of the
    /// given clock, to a `Platform::Instant` suitable for use as a deadline.
    ///
    /// If the time is so far in the future that it cannot be represented as an
    /// `Instant`, returns `Ok(None)`. If the time occurs in the past, returns
    /// the current time.
    fn duration_since_epoch_to_deadline(
        &self,
        clock_id: litebox_common_linux::ClockId,
        duration: Duration,
    ) -> Result<Option<<Platform as TimeProvider>::Instant>, Errno> {
        match clock_id {
            litebox_common_linux::ClockId::Monotonic
            | litebox_common_linux::ClockId::MonotonicCoarse
            | litebox_common_linux::ClockId::MonotonicRaw
            | litebox_common_linux::ClockId::Boottime => {
                // No need to compute the current time since the offset from the
                // request to `Instant` is known.
                Ok(self.global.boot_time.checked_add(duration))
            }
            _ => {
                // Convert between time domains. If the requested time is in the past,
                // return the current time.
                let current_time = self.gettime_as_duration(clock_id)?;
                Ok(self
                    .global
                    .platform
                    .now()
                    .checked_add(duration.checked_sub(current_time).unwrap_or(Duration::ZERO)))
            }
        }
    }

    /// Handle syscall `clock_getres`.
    pub(crate) fn sys_clock_getres(
        &self,
        clockid: litebox_common_linux::ClockId,
        res: TimeParam,
    ) -> Result<(), Errno> {
        // Return the resolution of the clock
        let resolution = match clockid {
            litebox_common_linux::ClockId::MonotonicCoarse
            | litebox_common_linux::ClockId::RealTimeCoarse => {
                // Coarse clocks typically have lower resolution (e.g., 4 millisecond)
                Duration::from_millis(4)
            }
            litebox_common_linux::ClockId::RealTime
            | litebox_common_linux::ClockId::Monotonic
            | litebox_common_linux::ClockId::MonotonicRaw
            | litebox_common_linux::ClockId::Boottime
            | litebox_common_linux::ClockId::ProcessCputimeId
            | litebox_common_linux::ClockId::ThreadCputimeId => {
                // For most modern systems, the resolution is typically 1 nanosecond
                // This is a reasonable default for high-resolution timers
                Duration::from_nanos(1)
            }
            _ => {
                log_unsupported!("getres for {clockid:?}");
                return Err(Errno::EINVAL);
            }
        };

        res.write::<Platform>(resolution)
    }

    /// Handle syscall `clock_nanosleep`.
    pub(crate) fn sys_clock_nanosleep(
        &self,
        clockid: litebox_common_linux::ClockId,
        flags: litebox_common_linux::TimerFlags,
        request: TimeParam,
        remain: TimeParam,
    ) -> Result<(), Errno> {
        let request = request.read::<Platform>()?.ok_or(Errno::EFAULT)?;
        if flags.intersects(litebox_common_linux::TimerFlags::ABSTIME.complement()) {
            return Err(Errno::EINVAL);
        }
        let is_abs = flags.contains(litebox_common_linux::TimerFlags::ABSTIME);

        // Set up a wait context with the right deadline/timeout.
        let wait_cx = self.wait_cx();
        let wait_cx = if is_abs {
            wait_cx.with_deadline(self.duration_since_epoch_to_deadline(clockid, request)?)
        } else {
            // Relative. Treat all clocks the same. TODO: handle the different clocks differently.
            wait_cx.with_timeout(request)
        };

        match wait_cx.sleep() {
            WaitError::TimedOut => {}
            WaitError::Interrupted => {
                if is_abs {
                    return Err(Errno::EINTR);
                }
                if let Some(remaining_timeout) = wait_cx.remaining_timeout() {
                    remain.write::<Platform>(remaining_timeout)?;
                    return Err(Errno::EINTR);
                }
                // Whoops, time ran out after getting interrupted. Treat this as a timeout.
            }
        }

        Ok(())
    }

    /// Handle syscall `gettimeofday`.
    pub(crate) fn sys_gettimeofday(
        &self,
        tv: Option<UserPtrMut<litebox_common_linux::TimeVal>>,
        tz: Option<UserPtrMut<litebox_common_linux::TimeZone>>,
    ) -> Result<(), Errno> {
        if let Some(tz) = tz {
            // `man 2 gettimeofday`: The use of the timezone structure is obsolete; the tz argument
            // should normally be specified as NULL. Linux still accepts a non-NULL tz and fills it
            // in (typically with zeros for UTC systems) rather than returning an error.
            let utc_tz = litebox_common_linux::TimeZone::new(0, 0);
            tz.write_at_offset::<Platform>(0, utc_tz)
                .ok_or(Errno::EFAULT)?;
        }
        if let Some(tv) = tv {
            tv.write_at_offset::<Platform>(0, self.real_time_as_duration_since_epoch().into())
                .ok_or(Errno::EFAULT)?;
        }
        Ok(())
    }

    /// Handle syscall `time`.
    pub(crate) fn sys_time(
        &self,
        tloc: Option<UserPtrMut<litebox_common_linux::time_t>>,
    ) -> Result<litebox_common_linux::time_t, Errno> {
        let time = self.real_time_as_duration_since_epoch();
        let seconds: u64 = time.as_secs();
        let seconds: litebox_common_linux::time_t = seconds.try_into().or(Err(Errno::EOVERFLOW))?;
        if let Some(tloc) = tloc {
            tloc.write_at_offset::<Platform>(0, seconds)
                .ok_or(Errno::EFAULT)?;
        }
        Ok(seconds)
    }

    /// Handle syscall `alarm`.
    ///
    /// Sets a process-wide timer to deliver SIGALRM after `seconds` seconds. If
    /// `seconds` is 0, any pending alarm is cancelled. Returns the number of
    /// seconds remaining on a previously set alarm (rounded up), or 0 if none
    /// was set.
    ///
    /// The alarm is per-process: all threads share the same alarm timer.
    pub(crate) fn sys_alarm(&self, seconds: u32) -> Result<u32, Errno> {
        let prev = self.arm_real_timer(Duration::from_secs(u64::from(seconds)))?;
        // Round remaining time up to whole seconds, saturating to u32::MAX.
        if prev.is_zero() {
            Ok(0)
        } else {
            let extra = u64::from(prev.subsec_nanos() > 0);
            Ok(u32::try_from(prev.as_secs() + extra).unwrap_or(u32::MAX))
        }
    }

    /// Arm or disarm the per-process `ITIMER_REAL` timer. Returns the raw
    /// `Duration` remaining on the previous arming; zero means "was not
    /// armed". `delay = 0` disarms.
    fn arm_real_timer(&self, delay: Duration) -> Result<Duration, Errno> {
        let mut alarm = self.process().alarm_timer.lock();
        let now = self.global.platform.now();
        let prev = alarm.remaining(now);
        let new_deadline = if delay.is_zero() {
            None
        } else {
            Some(now.checked_add(delay).ok_or(Errno::EINVAL)?)
        };
        if alarm.handle.is_none() {
            match self
                .global
                .platform
                .create_timer(litebox_common_linux::signal::Signal::SIGALRM)
            {
                Ok(handle) => alarm.handle = Some(handle),
                Err(litebox::platform::TimerCreationError::Unsupported) => {}
                Err(_) => unimplemented!(),
            }
        }
        if let Some(handle) = &alarm.handle {
            handle.set_timer(delay);
        }
        alarm.deadline = new_deadline;
        Ok(prev)
    }

    /// Handle syscall `setitimer`.
    pub(crate) fn sys_setitimer(
        &self,
        which: IntervalTimer,
        new_value: Option<UserPtr<ItimerVal>>,
        old_value: Option<UserPtrMut<ItimerVal>>,
    ) -> Result<(), Errno> {
        let new = match new_value {
            Some(ptr) => ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?,
            // Linux supports NULL `new_value` but says it would be removed in the future.
            None => ItimerVal::default(),
        };
        // tv_usec range check is performed by `Duration::try_from(TimeVal)`.
        let new_interval = Duration::try_from(new.it_interval())?;
        let new_remaining = Duration::try_from(new.it_value())?;

        let prev = match which {
            IntervalTimer::Real => {
                if new_remaining.is_zero() {
                    ItimerVal::single_shot(self.arm_real_timer(Duration::ZERO)?)
                } else if !new_interval.is_zero() {
                    // TODO: support periodic timers
                    log_unsupported!("setitimer: nonzero it_interval not supported");
                    return Err(Errno::ENOSYS);
                } else {
                    ItimerVal::single_shot(self.arm_real_timer(new_remaining)?)
                }
            }
            IntervalTimer::Virtual | IntervalTimer::Prof => {
                log_unsupported!("setitimer: ITIMER_VIRTUAL/PROF not supported");
                return Err(Errno::ENOSYS);
            }
        };

        if let Some(out) = old_value {
            out.write_at_offset::<Platform>(0, prev)
                .ok_or(Errno::EFAULT)?;
        }
        Ok(())
    }

    /// Handle syscall `getitimer`.
    pub(crate) fn sys_getitimer(
        &self,
        which: IntervalTimer,
        curr_value: UserPtrMut<ItimerVal>,
    ) -> Result<(), Errno> {
        let value = match which {
            IntervalTimer::Real => {
                let alarm = self.process().alarm_timer.lock();
                let now = self.global.platform.now();
                alarm.remaining(now)
            }
            IntervalTimer::Virtual | IntervalTimer::Prof => {
                log_unsupported!("getitimer: ITIMER_VIRTUAL/PROF not supported");
                Duration::ZERO
            }
        };
        curr_value
            .write_at_offset::<Platform>(0, ItimerVal::single_shot(value))
            .ok_or(Errno::EFAULT)
    }

    /// Handle syscall `pause`.
    pub(crate) fn sys_pause(&self) -> Result<(), Errno> {
        match self.wait_cx().sleep() {
            WaitError::Interrupted => Err(Errno::EINTR),
            WaitError::TimedOut => unreachable!("pause sleep has no deadline"),
        }
    }

    /// Handle syscall `getpid`.
    pub(crate) fn sys_getpid(&self) -> i32 {
        self.pid
    }

    pub(crate) fn sys_getppid(&self) -> i32 {
        self.ppid
    }

    /// Handle syscall `getpgid`.
    ///
    /// We have no global pid registry (see `do_kill`'s doc comment), so `pid` may only name the
    /// calling process itself (`0`, or the caller's own pid) or a live direct child (reachable
    /// via `children`, the same reachability `do_kill`'s remote-child case relies on) --
    /// matching real Linux's `ESRCH` for any other pid, since there is nowhere to look one up.
    pub(crate) fn sys_getpgid(&self, pid: i32) -> Result<i32, Errno> {
        if pid == 0 || pid == self.pid {
            Ok(self.process().pgid.load(Ordering::Relaxed))
        } else if pid > 0 {
            self.process()
                .find_child(pid)
                .map(|child| child.pgid.load(Ordering::Relaxed))
                .ok_or(Errno::ESRCH)
        } else {
            Err(Errno::ESRCH)
        }
    }

    /// Handle syscall `setpgid`.
    ///
    /// Real Linux additionally restricts `setpgid` to processes within the same session and
    /// forbids changing the pgid of a process that has already called `execve` (`EACCES`); we
    /// don't model sessions or "has this process execve'd yet" at all, so those checks are not
    /// enforced -- only the pid-target restriction (see [`Self::sys_getpgid`]) and `EINVAL` for a
    /// negative `pgid` are. `pid` may target a live direct child, not just self -- the standard
    /// shell-job-control pattern of a parent shell moving a freshly forked-but-not-yet-exec'd
    /// child into a (possibly brand new) pipeline process group before letting it run.
    pub(crate) fn sys_setpgid(&self, pid: i32, requested_group: i32) -> Result<(), Errno> {
        if requested_group < 0 {
            return Err(Errno::EINVAL);
        }
        let (target_process, target_own_pid) = if pid == 0 || pid == self.pid {
            (self.process().clone(), self.pid)
        } else if pid > 0 {
            let child = self.process().find_child(pid).ok_or(Errno::ESRCH)?;
            (child, pid)
        } else {
            return Err(Errno::ESRCH);
        };
        let target_pgid = if requested_group == 0 {
            target_own_pid
        } else {
            requested_group
        };
        target_process.pgid.store(target_pgid, Ordering::Relaxed);
        Ok(())
    }

    /// Handle syscall `setsid`.
    ///
    /// Real Linux fails with `EPERM` if the caller is already a process group leader (a session
    /// leader always is). We don't model sessions or true parent/child pgid inheritance at all
    /// (see `sys_setpgid`'s doc comment) -- and *every* process here starts out as its own
    /// process-group leader by construction (`Process::new` seeds `pgid` with the process's own
    /// pid) -- so enforcing that check faithfully would make `setsid()` unconditionally fail for
    /// exactly the caller that most needs it to succeed: a freshly `fork()`ed child running
    /// glibc's `login_tty()` (the primitive under `forkpty()`/`openpty()`-based tools --
    /// node-pty, Python's `os.forkpty()`, tmux, `script`), which always calls `setsid()`
    /// immediately after `fork()` and before anything else. Matching this build's existing
    /// "accept and remember" idiom for state it doesn't fully model, this always succeeds and
    /// makes the caller its own process-group leader (mirroring `setpgid(0, 0)`), returning its
    /// pid as the new session id (session id == pid is exactly true for a real session leader,
    /// which this is standing in for).
    #[expect(
        clippy::unnecessary_wraps,
        reason = "keeps the real syscall's fallible signature (matching sys_setpgid/sys_getpgid) rather than baking in that this build never rejects it, since that's a simplification of the real ABI, not a guarantee"
    )]
    pub(crate) fn sys_setsid(&self) -> Result<i32, Errno> {
        self.process().pgid.store(self.pid, Ordering::Relaxed);
        Ok(self.pid)
    }

    /// Handle syscall `getuid`.
    pub(crate) fn sys_getuid(&self) -> u32 {
        self.credentials.uid
    }

    /// Handle syscall `geteuid`.
    pub(crate) fn sys_geteuid(&self) -> u32 {
        self.credentials.euid
    }

    /// Handle syscall `getgid`.
    pub(crate) fn sys_getgid(&self) -> u32 {
        self.credentials.gid
    }

    /// Handle syscall `getegid`.
    pub(crate) fn sys_getegid(&self) -> u32 {
        self.credentials.egid
    }

    /// Handle syscall `setuid`.
    ///
    /// LiteBox does not support real privilege separation (there is exactly one, fixed set of
    /// credentials for the whole sandboxed guest), so this succeeds as a no-op when `uid`
    /// matches the caller's current real/effective uid -- the common case of a program
    /// idempotently dropping to the uid it is already running as -- and fails otherwise, rather
    /// than silently pretending to change privileges.
    pub(crate) fn sys_setuid(&self, uid: u32) -> Result<(), Errno> {
        if uid == self.credentials.uid && uid == self.credentials.euid {
            Ok(())
        } else {
            Err(Errno::EPERM)
        }
    }

    /// Handle syscall `setgid`. See [`Self::sys_setuid`] for the same no-op-if-unchanged
    /// rationale.
    pub(crate) fn sys_setgid(&self, gid: u32) -> Result<(), Errno> {
        if gid == self.credentials.gid && gid == self.credentials.egid {
            Ok(())
        } else {
            Err(Errno::EPERM)
        }
    }
}

/// Number of CPUs
const NR_CPUS: usize = 2;

pub(crate) struct CpuSet {
    bits: bitvec::vec::BitVec<u8>,
}

impl CpuSet {
    pub(crate) fn len(&self) -> usize {
        self.bits.len()
    }
    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.bits.as_raw_slice()
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Handle syscall `sched_getaffinity`.
    ///
    /// Note this is a dummy implementation that always returns the same CPU set
    pub(crate) fn sys_sched_getaffinity(&self, _pid: Option<i32>) -> CpuSet {
        let mut cpuset = bitvec::bitvec![u8, bitvec::order::Lsb0; 0; NR_CPUS];
        cpuset.iter_mut().for_each(|mut b| *b = true);
        CpuSet { bits: cpuset }
    }
}

/// `SCHED_OTHER` (aka `SCHED_NORMAL`), Linux's default scheduling policy. Under this policy
/// `sched_priority` is always `0` -- static priority is only meaningful for the real-time
/// policies (`SCHED_FIFO`/`SCHED_RR`), which litebox does not actually implement.
pub(crate) const SCHED_OTHER: i32 = 0;

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Handle syscall `sched_getparam`.
    ///
    /// We don't implement real OS-level scheduling; this returns whatever `sched_priority` was
    /// last recorded via `sched_setparam`/`sched_setscheduler` for this thread (`0` under the
    /// default `SCHED_OTHER` policy, matching real Linux).
    pub(crate) fn sys_sched_getparam(&self, _pid: Option<i32>) -> i32 {
        self.thread.sched_policy_priority.get().1
    }

    /// Handle syscall `sched_setparam`. Accept-and-remember: records `sched_priority` without
    /// implementing real scheduling semantics.
    pub(crate) fn sys_sched_setparam(&self, _pid: Option<i32>, sched_priority: i32) {
        let (policy, _) = self.thread.sched_policy_priority.get();
        self.thread
            .sched_policy_priority
            .set((policy, sched_priority));
    }

    /// Handle syscall `sched_getscheduler`. See [`Self::sys_sched_getparam`].
    pub(crate) fn sys_sched_getscheduler(&self, _pid: Option<i32>) -> i32 {
        self.thread.sched_policy_priority.get().0
    }

    /// Handle syscall `sched_setscheduler`. Accept-and-remember: records the policy and priority
    /// without implementing real scheduling semantics. See [`Self::sys_sched_getparam`].
    pub(crate) fn sys_sched_setscheduler(
        &self,
        _pid: Option<i32>,
        policy: i32,
        sched_priority: i32,
    ) {
        self.thread
            .sched_policy_priority
            .set((policy, sched_priority));
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Handle syscall `futex`
    pub(crate) fn sys_futex(&self, arg: litebox_common_linux::FutexArgs) -> Result<usize, Errno> {
        /// Note our mutex implementation assumes futexes are private as we don't support shared memory yet.
        /// It should be fine to treat shared futexes as private for now.
        macro_rules! warn_shared_futex {
            ($flag:ident) => {
                if !$flag.contains(litebox_common_linux::FutexFlags::PRIVATE) {
                    log_unsupported!("shared futex");
                }
            };
        }

        let res = match arg {
            FutexArgs::Wake { addr, flags, count } => {
                warn_shared_futex!(flags);
                let Some(count) = core::num::NonZeroU32::new(count) else {
                    return Ok(0);
                };
                let woken = self.global.futex_manager.wake(
                    addr.to_platform_ptr::<Platform>(),
                    count,
                    None,
                )? as usize;
                litebox_util_log::trace!(
                    tid:% = self.tid,
                    addr:% = addr.as_usize(),
                    requested:% = count.get(),
                    woken:% = woken;
                    "futex: WAKE"
                );
                woken
            }
            FutexArgs::Wait {
                addr,
                flags,
                val,
                timeout,
            } => {
                warn_shared_futex!(flags);
                let timeout = timeout.read::<Platform>()?;
                litebox_util_log::trace!(
                    tid:% = self.tid,
                    addr:% = addr.as_usize(),
                    val:% = val,
                    timeout:? = timeout;
                    "futex: WAIT enter"
                );
                let res = self.global.futex_manager.wait(
                    &self.wait_cx().with_timeout(timeout),
                    addr.to_platform_ptr::<Platform>(),
                    val,
                    None,
                );
                litebox_util_log::trace!(
                    tid:% = self.tid,
                    addr:% = addr.as_usize(),
                    res:? = res;
                    "futex: WAIT return"
                );
                res?;
                0
            }
            litebox_common_linux::FutexArgs::WaitBitset {
                addr,
                flags,
                val,
                timeout,
                bitmask,
            } => {
                warn_shared_futex!(flags);
                let deadline = if let Some(timeout) = timeout.read::<Platform>()? {
                    let clock_id =
                        if flags.contains(litebox_common_linux::FutexFlags::CLOCK_REALTIME) {
                            litebox_common_linux::ClockId::RealTime
                        } else {
                            litebox_common_linux::ClockId::Monotonic
                        };
                    self.duration_since_epoch_to_deadline(clock_id, timeout)?
                } else {
                    None
                };
                self.global.futex_manager.wait(
                    &self.wait_cx().with_deadline(deadline),
                    addr.to_platform_ptr::<Platform>(),
                    val,
                    core::num::NonZeroU32::new(bitmask),
                )?;
                0
            }
            litebox_common_linux::FutexArgs::Requeue {
                addr,
                flags,
                wake_count,
                requeue_count,
                addr2,
            } => {
                warn_shared_futex!(flags);
                let woken = self.global.futex_manager.requeue(
                    addr.to_platform_ptr::<Platform>(),
                    wake_count,
                    requeue_count,
                    addr2.to_platform_ptr::<Platform>(),
                    None,
                )? as usize;
                litebox_util_log::trace!(
                    tid:% = self.tid,
                    addr:% = addr.as_usize(),
                    addr2:% = addr2.as_usize(),
                    wake_count:% = wake_count,
                    requeue_count:% = requeue_count,
                    woken:% = woken;
                    "futex: REQUEUE"
                );
                woken
            }
            litebox_common_linux::FutexArgs::CmpRequeue {
                addr,
                flags,
                wake_count,
                requeue_count,
                addr2,
                expected_value,
            } => {
                warn_shared_futex!(flags);
                let woken = self.global.futex_manager.requeue(
                    addr.to_platform_ptr::<Platform>(),
                    wake_count,
                    requeue_count,
                    addr2.to_platform_ptr::<Platform>(),
                    Some(expected_value),
                )? as usize;
                litebox_util_log::trace!(
                    tid:% = self.tid,
                    addr:% = addr.as_usize(),
                    addr2:% = addr2.as_usize(),
                    wake_count:% = wake_count,
                    requeue_count:% = requeue_count,
                    woken:% = woken;
                    "futex: CMP_REQUEUE"
                );
                woken
            }
            _ => unimplemented!("Unsupported futex operation"),
        };
        Ok(res)
    }
}

const MAX_VEC: usize = 4096; // limit count
const MAX_TOTAL_BYTES: usize = 256 * 1024; // size cap

/// Maximum shebang (#!) recursion depth (from Linux's `exec_binprm`)
const SHEBANG_MAX_RECURSION: u32 = 6;

/// Maximum length of a shebang line that we inspect. Matches Linux `BINPRM_BUF_SIZE`.
const SHEBANG_MAX_LINE: usize = 256;

/// Parse a `#!interpreter [optional-arg]` line from a file header buffer.
///
/// Returns `Some((interpreter, optional_arg))` when `buf` starts with `#!` and
/// contains a non-empty interpreter path. The optional argument, if present, is everything
/// between the first whitespace after the interpreter and the end of the line
/// (trimmed), treated as a single token — matching Linux kernel semantics.
fn parse_shebang(buf: &[u8]) -> Option<(&str, Option<&str>)> {
    if buf.len() < 2 || buf[0] != b'#' || buf[1] != b'!' {
        return None;
    }
    let line_end = buf[2..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(buf.len(), |p| p + 2);
    let line = core::str::from_utf8(&buf[2..line_end]).ok()?;
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    match line.find([' ', '\t']) {
        Some(i) => {
            let arg = line[i..].trim();
            Some((&line[..i], if arg.is_empty() { None } else { Some(arg) }))
        }
        None => Some((line, None)),
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Resolve shebang (`#!`) chains for the given path and argv if the file starts with a shebang line.
    /// Otherwise, returns the original path and argv.
    pub(crate) fn resolve_shebang(
        &self,
        mut path: alloc::string::String,
        mut argv: alloc::vec::Vec<alloc::ffi::CString>,
    ) -> Result<(alloc::string::String, alloc::vec::Vec<alloc::ffi::CString>), Errno> {
        for _ in 0..SHEBANG_MAX_RECURSION {
            let full_path = self.resolve_path(&path)?;
            let file = self.do_open(
                full_path,
                litebox::fs::OFlags::RDONLY,
                litebox::fs::Mode::empty(),
            )?;
            let mut header = [0u8; SHEBANG_MAX_LINE];
            let files = self.files.borrow();
            let n = match files.fs.read(&file, &mut header, Some(0)) {
                Ok(n) => n,
                Err(e) => {
                    let _ = files.fs.close(&file);
                    return Err(Errno::from(e));
                }
            };
            let _ = files.fs.close(&file);

            match parse_shebang(&header[..n]) {
                Some((interp, opt_arg)) => {
                    let mut new_argv = alloc::vec::Vec::new();
                    new_argv.push(alloc::ffi::CString::new(interp).map_err(|_| Errno::EINVAL)?);
                    if let Some(arg) = opt_arg {
                        new_argv.push(alloc::ffi::CString::new(arg).map_err(|_| Errno::EINVAL)?);
                    }
                    new_argv
                        .push(alloc::ffi::CString::new(path.as_str()).map_err(|_| Errno::EINVAL)?);
                    if argv.len() > 1 {
                        new_argv.extend_from_slice(&argv[1..]);
                    }
                    path = alloc::string::String::from(interp);
                    argv = new_argv;
                }
                None => return Ok((path, argv)),
            }
        }
        Err(Errno::ELOOP)
    }

    /// Handle syscall `execve`.
    pub(crate) fn sys_execve(
        &self,
        pathname: UserPtr<i8>,
        argv: UserPtr<UserPtr<i8>>,
        envp: UserPtr<UserPtr<i8>>,
        ctx: &mut litebox_common_linux::PtRegs,
    ) -> Result<usize, Errno> {
        fn copy_vector<Platform: ShimPlatform>(
            mut base: UserPtr<UserPtr<i8>>,
            which: &str,
        ) -> Result<alloc::vec::Vec<alloc::ffi::CString>, Errno> {
            let mut out = alloc::vec::Vec::new();
            let mut total = 0usize;
            for _ in 0..MAX_VEC {
                let p: UserPtr<i8> = {
                    // read pointer-sized entries
                    match base.read_at_offset::<Platform>(0) {
                        Some(ptr) => ptr,
                        None => return Err(Errno::EFAULT),
                    }
                };
                if p.as_usize() == 0 {
                    break;
                }
                let Some(cs) = p.to_cstring::<Platform>() else {
                    return Err(Errno::EFAULT);
                };
                total += cs.as_bytes().len() + 1;
                if total > MAX_TOTAL_BYTES {
                    return Err(Errno::E2BIG);
                }
                litebox_util_log::trace!(
                    which = which,
                    idx:% = out.len(),
                    ptr:% = p.as_usize(),
                    len:% = cs.as_bytes().len(),
                    bytes:? = cs.as_bytes();
                    "execve: copied argv/envp entry"
                );
                out.push(cs);
                // advance to next pointer
                base = UserPtr::from_usize(base.as_usize() + core::mem::size_of::<usize>());
            }
            Ok(out)
        }

        // `execve` replaces the address space wholesale: any stale pointer into the parent's
        // pre-`fork()` ranges is unreachable from here on, so post-`fork()` verification (if it
        // was armed for this thread) has served its purpose and must stop.
        self.global.platform.end_fork_child_verification();

        // Copy pathname
        let Some(path_cstr) = pathname.to_cstring::<Platform>() else {
            return Err(Errno::EFAULT);
        };
        let path = path_cstr.to_str().map_err(|_| Errno::ENOENT)?;

        litebox_util_log::debug!(tid:% = self.tid, path:% = path; "sys_execve: entry");

        // Copy argv and envp vectors
        let argv_vec = if argv.as_usize() == 0 {
            alloc::vec::Vec::new()
        } else {
            copy_vector::<Platform>(argv, "argv")?
        };
        let envp_vec = if envp.as_usize() == 0 {
            alloc::vec::Vec::new()
        } else {
            copy_vector::<Platform>(envp, "envp")?
        };

        let (path, argv_vec) = self.resolve_shebang(alloc::string::String::from(path), argv_vec)?;

        let loader = crate::loader::elf::ElfLoader::new(self, &path)?;

        // After this point, the old program is torn down and failures must terminate the process.

        // Kill all the other threads in this process and wait for them to exit.
        if !self.kill_other_threads() {
            // Another thread is already in the process of execve. This thread
            // will exit; return any error code.
            return Err(Errno::EBUSY);
        }

        // Close CLOEXEC descriptors
        self.close_on_exec();

        // unmmap all memory mappings and reset brk
        if let Some(robust_list) = self.thread.robust_list.take() {
            let _ = self.wake_robust_list(robust_list);
        }
        self.thread.clear_child_tid.set(None);

        self.signals.reset_for_exec();

        // Don't release reserved mappings.
        let release = |_r: Range<usize>, vm: VmFlags| !vm.is_empty();
        unsafe { self.process().pm.release_memory(release) }
            .expect("failed to release memory mappings");

        self.global
            .platform
            .set_arch_specific_register(&ArchSpecificRegister::FsBase, 0)
            .expect("failed to clear guest TLS on execve");

        self.load_program(loader, argv_vec, envp_vec)
            .expect("TODO: terminate the process cleanly");

        // If this process was created via `vfork()`, this is the point its POSIX-mandated
        // parent suspension ends (see `Process::wait_for_vfork_done`'s doc comment). A no-op for
        // a plain `fork()`ed or never-vforked process.
        self.process().signal_vfork_done();

        self.init_thread_context(ctx);
        Ok(0)
    }

    /// Loads the specified program into the process's address space and prepares the thread
    /// to start executing it.
    pub(crate) fn load_program(
        &self,
        mut loader: crate::loader::elf::ElfLoader<'_, Platform, FS>,
        argv: Vec<alloc::ffi::CString>,
        envp: Vec<alloc::ffi::CString>,
    ) -> Result<(), crate::loader::elf::ElfLoaderError> {
        let load_info = loader.load(argv, envp, self.init_auxv())?;

        self.set_task_comm(loader.comm());

        self.thread
            .init_state
            .set(ThreadInitState::NewProcess(load_info));
        Ok(())
    }

    pub(crate) fn handle_init_request(&self, ctx: &mut litebox_common_linux::PtRegs) {
        self.init_thread_context(ctx);
        // Attach the thread handle so that the thread can be interrupted.
        self.thread
            .remote
            .handle
            .set(Box::new(self.wait_state.thread_handle()))
            .ok();
    }

    /// Initialize the thread context for a new process or thread, and perform any
    /// other initial setup required.
    fn init_thread_context(&self, ctx: &mut litebox_common_linux::PtRegs) {
        match self.thread.init_state.take() {
            ThreadInitState::None => {}
            ThreadInitState::NewProcess(load_info) => {
                #[cfg(target_arch = "x86_64")]
                {
                    *ctx = litebox_common_linux::PtRegs {
                        r15: 0,
                        r14: 0,
                        r13: 0,
                        r12: 0,
                        rbp: 0,
                        rbx: 0,
                        r11: 0,
                        r10: 0,
                        r9: 0,
                        r8: 0,
                        rax: 0,
                        rcx: 0,
                        rdx: 0,
                        rsi: 0,
                        rdi: 0,
                        orig_rax: 0,
                        rip: load_info.entry_point,
                        cs: 0x33, // __USER_CS
                        eflags: 0,
                        rsp: load_info.user_stack_top,
                        ss: 0x2b, // __USER_DS
                    };
                }
            }
            ThreadInitState::NewThread {
                tls,
                stack,
                set_child_tid,
            } => {
                // Set the stack and the return value from clone().
                #[cfg(target_arch = "x86_64")]
                {
                    if let Some(stack) = stack {
                        ctx.rsp = stack;
                    }
                    ctx.rax = 0;
                }

                // Set the TLS for the new thread.
                if let Some(tls) = tls {
                    #[cfg(target_arch = "x86_64")]
                    {
                        self.sys_arch_prctl(ArchPrctlArg::SetFs(tls.as_usize()))
                            .unwrap();
                    }
                }

                if let Some(child_tid_ptr) = set_child_tid {
                    // Set the child TID if requested.
                    let _ = child_tid_ptr.write_at_offset::<Platform>(0, self.tid);
                }
            }
            #[cfg_attr(not(target_arch = "x86_64"), expect(unused_variables))]
            ThreadInitState::ForkedChild(mut parent_ctx, fs_base, relocations) => {
                #[cfg(target_arch = "x86_64")]
                {
                    parent_ctx.rax = 0;
                    self.sys_arch_prctl(ArchPrctlArg::SetFs(fs_base)).unwrap();
                }
                *ctx = parent_ctx;
                // This runs on the child's own (brand-new) host thread, immediately before it
                // first resumes into guest code -- ask the platform to verify that the child
                // never executes at, nor writes through, a stale pointer into the parent's
                // pre-`fork()` address space. See `ForkChildVerificationProvider`.
                self.global
                    .platform
                    .begin_fork_child_verification(relocations);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{UserPtr, UserPtrMut};
    use litebox_common_linux::errno::Errno;

    extern crate std;

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_arch_prctl() {
        use crate::syscalls::tests::init_platform;
        use litebox_common_linux::ArchPrctlArg;

        let task = init_platform(None);

        // Save old FS base
        let mut old_fs_base: usize = 0;
        let ptr = UserPtrMut::from_ptr(&raw mut old_fs_base);
        task.sys_arch_prctl(ArchPrctlArg::GetFs(ptr))
            .expect("Failed to get FS base");

        // Set new FS base
        let mut new_fs_base: [u8; 16] = [0; 16];
        let ptr = UserPtrMut::from_ptr(new_fs_base.as_mut_ptr());
        task.sys_arch_prctl(ArchPrctlArg::SetFs(ptr.as_usize()))
            .expect("Failed to set FS base");

        // Verify new FS base
        let mut current_fs_base: usize = 0;
        let ptr = UserPtrMut::from_ptr(&raw mut current_fs_base);
        task.sys_arch_prctl(ArchPrctlArg::GetFs(ptr))
            .expect("Failed to get FS base");
        assert_eq!(current_fs_base, new_fs_base.as_ptr() as usize);

        // Restore old FS base
        let ptr: UserPtrMut<u8> = UserPtrMut::from_usize(old_fs_base);
        task.sys_arch_prctl(ArchPrctlArg::SetFs(ptr.as_usize()))
            .expect("Failed to restore FS base");
    }

    #[test]
    fn test_gettime_process_cputime_id_succeeds() {
        let task = crate::syscalls::tests::init_platform(None);
        let duration = task
            .gettime_as_duration(litebox_common_linux::ClockId::ProcessCputimeId)
            .expect("CLOCK_PROCESS_CPUTIME_ID must be supported");
        assert!(duration.as_nanos() < u128::MAX);
    }

    #[test]
    fn test_gettime_thread_cputime_id_succeeds() {
        let task = crate::syscalls::tests::init_platform(None);
        let duration = task
            .gettime_as_duration(litebox_common_linux::ClockId::ThreadCputimeId)
            .expect("CLOCK_THREAD_CPUTIME_ID must be supported");
        assert!(duration.as_nanos() < u128::MAX);
    }

    #[test]
    fn test_gettime_monotonic_raw_succeeds() {
        let task = crate::syscalls::tests::init_platform(None);
        task.gettime_as_duration(litebox_common_linux::ClockId::MonotonicRaw)
            .expect("CLOCK_MONOTONIC_RAW must be supported");
    }

    #[test]
    fn test_gettime_realtime_coarse_succeeds() {
        let task = crate::syscalls::tests::init_platform(None);
        task.gettime_as_duration(litebox_common_linux::ClockId::RealTimeCoarse)
            .expect("CLOCK_REALTIME_COARSE must be supported");
    }

    #[test]
    fn test_gettime_boottime_succeeds() {
        let task = crate::syscalls::tests::init_platform(None);
        task.gettime_as_duration(litebox_common_linux::ClockId::Boottime)
            .expect("CLOCK_BOOTTIME must be supported");
    }

    #[test]
    fn test_clock_id_invalid_value_rejected() {
        use litebox_common_linux::ClockId;
        use std::convert::TryFrom;

        assert!(ClockId::try_from(99i32).is_err());
    }

    #[test]
    fn test_clock_getres_process_cputime_id_succeeds() {
        use litebox_common_linux::Timespec;

        let task = crate::syscalls::tests::init_platform(None);
        let mut res = Timespec::default();
        task.sys_clock_getres(
            litebox_common_linux::ClockId::ProcessCputimeId,
            litebox_common_linux::TimeParam::Timespec64(UserPtrMut::from_ptr(&raw mut res)),
        )
        .expect("clock_getres on CLOCK_PROCESS_CPUTIME_ID must be supported");
    }

    #[test]
    fn test_sched_getaffinity() {
        let task = crate::syscalls::tests::init_platform(None);

        let cpuset = task.sys_sched_getaffinity(None);
        assert_eq!(cpuset.bits.len(), super::NR_CPUS);
        cpuset.bits.iter().for_each(|b| assert!(*b));
        let ones: usize = cpuset
            .as_bytes()
            .iter()
            .map(|b| b.count_ones() as usize)
            .sum();
        assert_eq!(ones, super::NR_CPUS);
    }

    #[test]
    fn test_sched_getparam_default_is_zero() {
        let task = crate::syscalls::tests::init_platform(None);

        assert_eq!(task.sys_sched_getparam(None), 0);
        assert_eq!(task.sys_sched_getparam(Some(0)), 0);
    }

    #[test]
    fn test_sched_getscheduler_default_is_sched_other() {
        let task = crate::syscalls::tests::init_platform(None);

        assert_eq!(task.sys_sched_getscheduler(None), super::SCHED_OTHER);
    }

    #[test]
    fn test_sched_setparam_then_getparam_roundtrip() {
        let task = crate::syscalls::tests::init_platform(None);

        task.sys_sched_setparam(None, 5);
        assert_eq!(task.sys_sched_getparam(None), 5);
    }

    #[test]
    fn test_sched_setscheduler_then_getscheduler_roundtrip() {
        const SCHED_FIFO: i32 = 1;
        let task = crate::syscalls::tests::init_platform(None);

        task.sys_sched_setscheduler(None, SCHED_FIFO, 10);
        assert_eq!(task.sys_sched_getscheduler(None), SCHED_FIFO);
        assert_eq!(task.sys_sched_getparam(None), 10);
    }

    #[test]
    fn test_getpgid_default_is_own_pid() {
        let task = crate::syscalls::tests::init_platform(None);

        let pid = task.sys_getpid();
        assert_eq!(task.sys_getpgid(0), Ok(pid));
        assert_eq!(task.sys_getpgid(pid), Ok(pid));
    }

    #[test]
    fn test_setpgid_then_getpgid_roundtrip() {
        let task = crate::syscalls::tests::init_platform(None);
        let pid = task.sys_getpid();

        assert_eq!(task.sys_setpgid(0, 4242), Ok(()));
        assert_eq!(task.sys_getpgid(0), Ok(4242));
        assert_eq!(task.sys_getpgid(pid), Ok(4242));
    }

    #[test]
    fn test_setpgid_zero_pgid_defaults_to_own_pid() {
        let task = crate::syscalls::tests::init_platform(None);
        let pid = task.sys_getpid();

        assert_eq!(task.sys_setpgid(0, 4242), Ok(()));
        assert_eq!(task.sys_setpgid(0, 0), Ok(()));
        assert_eq!(task.sys_getpgid(0), Ok(pid));
    }

    #[test]
    fn test_resource_limits_copy_from_inherits_parent_values() {
        // Regression test for fork() not inheriting rlimits: before this, every freshly
        // constructed `Process` (including every forked child) got `ResourceLimits::default()`
        // and nothing ever copied the parent's actual current limits into it, silently
        // discarding a `setrlimit()` the parent made before forking.
        use litebox_common_linux::RlimitResource;

        let parent = super::ResourceLimits::default();
        parent.set_rlimit(
            RlimitResource::NOFILE,
            litebox_common_linux::Rlimit {
                rlim_cur: 42,
                rlim_max: 100,
            },
        );

        let child = super::ResourceLimits::default();
        // Sanity: the child's own default differs from what we're about to inherit.
        assert_ne!(child.get_rlimit(RlimitResource::NOFILE).rlim_cur, 42);

        child.copy_from(&parent);
        let inherited = child.get_rlimit(RlimitResource::NOFILE);
        assert_eq!(inherited.rlim_cur, 42);
        assert_eq!(inherited.rlim_max, 100);
    }

    #[test]
    fn test_kill_own_pgid_zero_and_negative_deliver_to_self() {
        // `kill(0, sig)` (own process group), `kill(-pgid, sig)`, and `kill(-1, sig)`
        // (broadcast) used to unconditionally fail with ESRCH -- this shim has no registry of
        // other live processes, but self is always a genuine member of all three of those target
        // sets, so failing outright was needlessly wrong for what is likely the single most
        // common real caller: a script signaling its own process group during cleanup.
        use litebox_common_linux::PtRegs;
        use litebox_common_linux::signal::Signal;

        let task = crate::syscalls::tests::init_platform(None);
        let pgid = task.sys_getpgid(0).unwrap();

        assert_eq!(task.sys_kill(0, Signal::SIGUSR1.as_i32()), Ok(0));
        assert!(task.pending_signal_set().contains(Signal::SIGUSR1));

        // Drain it and try again via -pgid.
        let mut regs = PtRegs::default();
        task.process_signals(&mut regs);
        assert!(!task.has_pending_signals());

        assert_eq!(task.sys_kill(-pgid, Signal::SIGUSR2.as_i32()), Ok(0));
        assert!(task.pending_signal_set().contains(Signal::SIGUSR2));
    }

    #[test]
    fn test_kill_genuine_remote_pid_still_fails() {
        // A pid that is neither self, self's own process group, nor a direct child (the one
        // remote-process case `do_kill` can actually reach -- see the tests below) is a real,
        // specific target this shim genuinely cannot find (no shim-wide pid registry) -- reporting
        // that honestly (ESRCH) is correct, not a regression to "fix" by pretending to deliver it.
        use litebox_common_linux::signal::Signal;

        let task = crate::syscalls::tests::init_platform(None);
        let other_pid = task.sys_getpid().wrapping_add(1000);
        assert_eq!(
            task.sys_kill(other_pid, Signal::SIGUSR1.as_i32()),
            Err(Errno::ESRCH)
        );
    }

    #[test]
    fn test_kill_queues_signal_for_a_live_direct_child_without_touching_the_parent() {
        // The synchronous half of cross-process signal delivery: kill(child_pid, sig) from the
        // parent must land in the CHILD's own process-directed pending set, not the parent's, and
        // must not require the child to be actively running to be queued.
        use litebox_common_linux::signal::Signal;

        let task = crate::syscalls::tests::init_platform(None);
        let child = task.clone_as_forked_child_for_test();
        assert_ne!(
            child.pid, task.pid,
            "a forked child must be a genuinely different process"
        );

        assert_eq!(task.sys_kill(child.pid, Signal::SIGTERM.as_i32()), Ok(0));

        assert!(
            child.pending_signal_set().contains(Signal::SIGTERM),
            "the signal must be queued for the child"
        );
        assert!(
            task.pending_signal_set().is_empty(),
            "the parent's own pending set must be untouched by kill()ing its child"
        );
    }

    /// Regression test for the cross-process signal delivery slice added this round:
    /// `kill(child_pid, sig)` targeting a *live, currently-blocked* direct child must actually
    /// wake it (via `Process::interrupt_all_threads`, mirroring `exit_group`/`kill_other_threads`'s
    /// existing collect-then-interrupt pattern for same-process delivery), surfacing `EINTR` from
    /// whatever blocking syscall it was in -- not just silently sit in the child's queue until it
    /// happens to check again on its own. See `test_exit_group_wakes_thread_blocked_in_futex_wait`
    /// for the same interrupt mechanism exercised same-process; this is its cross-process sibling.
    #[test]
    fn test_kill_wakes_a_live_direct_child_blocked_in_futex_wait() {
        use litebox_common_linux::signal::Signal;

        let task = crate::syscalls::tests::init_platform(None);
        <crate::syscalls::tests::TestPlatform as litebox::platform::ThreadProvider>::run_test_thread(|| {
            let mut futex_word: u32 = 0;
            let futex_addr = (&raw mut futex_word) as usize;

            let child_task = task.clone_as_forked_child_for_test();
            let child_pid = child_task.pid;
            assert_ne!(child_pid, task.pid);

            let bg = std::thread::spawn(move || {
                <crate::syscalls::tests::TestPlatform as litebox::platform::ThreadProvider>::run_test_thread(|| {
                    // See `test_exit_group_wakes_thread_blocked_in_futex_wait`'s identical setup
                    // step for why this is required for `interrupt()` to reach this thread at all.
                    child_task.set_thread_handle_for_test();

                    let futex_ptr = UserPtrMut::from_usize(futex_addr);
                    let result = child_task.sys_futex(litebox_common_linux::FutexArgs::Wait {
                        addr: futex_ptr,
                        flags: litebox_common_linux::FutexFlags::PRIVATE,
                        val: 0,
                        timeout: litebox_common_linux::TimeParam::None,
                    });
                    assert_eq!(
                        result,
                        Err(litebox_common_linux::errno::Errno::EINTR),
                        "the child's blocking futex wait must be interrupted by the parent's kill()"
                    );
                });
            });

            // Give the child a real chance to enter the blocking wait before signaling it --
            // otherwise this would trivially pass even without cross-process wakeup, since the
            // signal would already be pending before the child's wait ever started blocking.
            std::thread::sleep(core::time::Duration::from_millis(50));

            assert_eq!(task.sys_kill(child_pid, Signal::SIGTERM.as_i32()), Ok(0));

            bg.join().expect("child thread panicked");
        });
    }

    #[test]
    fn test_kill_own_pgid_zero_also_reaches_a_child_moved_into_the_same_group() {
        // Regression test: a group-directed kill(0/-1/-pgid, sig) used to only ever reach self
        // (approximated, since this shim has no registry of arbitrary other processes) -- but a
        // live child that's been moved into the caller's own group via setpgid() (the standard
        // shell-job-control/process-supervisor pattern of putting a whole spawned pipeline into
        // one group) is reachable via `children`, and must now be signaled too, not just self.
        use litebox_common_linux::signal::Signal;

        let task = crate::syscalls::tests::init_platform(None);
        let child = task.clone_as_forked_child_for_test();

        // Move the child into the parent's own process group.
        child.sys_setpgid(0, task.pid).unwrap();
        assert_eq!(child.sys_getpgid(0).unwrap(), task.pid);

        assert_eq!(task.sys_kill(0, Signal::SIGTERM.as_i32()), Ok(0));

        assert!(
            task.pending_signal_set().contains(Signal::SIGTERM),
            "self must still be signaled"
        );
        assert!(
            child.pending_signal_set().contains(Signal::SIGTERM),
            "a child in the same group must be signaled too"
        );
    }

    #[test]
    fn test_kill_negative_pgid_with_no_reachable_members_returns_esrch() {
        // A group-directed kill() targeting a pgid that is neither the caller's own group nor
        // any reachable child's group has literally nothing this shim can deliver to -- must be
        // ESRCH (matching real Linux's behavior for a pgid with zero members), not a silently
        // reported success.
        use litebox_common_linux::signal::Signal;

        let task = crate::syscalls::tests::init_platform(None);
        let unrelated_group = task.sys_getpid().wrapping_add(999_999).max(1);
        assert_eq!(
            task.sys_kill(-unrelated_group, Signal::SIGTERM.as_i32()),
            Err(Errno::ESRCH)
        );
    }

    #[test]
    fn test_setsid_returns_own_pid_and_becomes_own_group_leader() {
        // Mirrors what glibc's login_tty() does immediately after fork(): setsid() must succeed
        // (not EPERM) and leave the caller as its own process-group leader, exactly the
        // precondition TIOCSCTTY needs to then succeed too.
        let task = crate::syscalls::tests::init_platform(None);
        let pid = task.sys_getpid();

        task.sys_setpgid(0, 4242).unwrap();
        assert_eq!(task.sys_getpgid(0), Ok(4242));

        assert_eq!(task.sys_setsid(), Ok(pid));
        assert_eq!(task.sys_getpgid(0), Ok(pid));
    }

    #[test]
    fn test_setpgid_rejects_negative_pgid() {
        let task = crate::syscalls::tests::init_platform(None);

        assert_eq!(task.sys_setpgid(0, -1), Err(Errno::EINVAL));
    }

    #[test]
    fn test_getpgid_setpgid_reject_remote_pid() {
        let task = crate::syscalls::tests::init_platform(None);
        let pid = task.sys_getpid();
        let other_pid = pid.wrapping_add(1000);

        assert_eq!(task.sys_getpgid(other_pid), Err(Errno::ESRCH));
        assert_eq!(task.sys_setpgid(other_pid, 4242), Err(Errno::ESRCH));
    }

    #[test]
    fn test_setpgid_and_getpgid_can_target_a_live_direct_child() {
        // Regression test: setpgid()/getpgid() used to reject any pid other than self
        // unconditionally, even a live direct child -- but a parent moving a freshly forked
        // child into a (possibly brand new) pipeline process group before it runs is the
        // standard shell-job-control pattern (e.g. bash setting up `cmd1 | cmd2 | cmd3`), and the
        // child *is* reachable via `children`, the same reachability `do_kill`'s remote-child
        // case already relies on.
        let task = crate::syscalls::tests::init_platform(None);
        let child = task.clone_as_forked_child_for_test();

        // A freshly forked child defaults to being its own group leader.
        assert_eq!(task.sys_getpgid(child.pid).unwrap(), child.pid);

        // Move the child into an explicit new group (as a shell would for a pipeline).
        assert_eq!(task.sys_setpgid(child.pid, 4242), Ok(()));
        assert_eq!(task.sys_getpgid(child.pid).unwrap(), 4242);
        // The child's own view of its pgid must agree.
        assert_eq!(child.sys_getpgid(0).unwrap(), 4242);
        // Self must be untouched.
        assert_eq!(task.sys_getpgid(0).unwrap(), task.pid);

        // pgid == 0 means "use the target pid's own pid", not the caller's.
        assert_eq!(task.sys_setpgid(child.pid, 0), Ok(()));
        assert_eq!(task.sys_getpgid(child.pid).unwrap(), child.pid);
    }

    #[test]
    fn test_prctl_set_get_name() {
        let task = crate::syscalls::tests::init_platform(None);

        // Prepare a null-terminated name to set
        let name: &[u8] = b"litebox-test\0";

        // Call prctl(PR_SET_NAME, set_buf)
        let set_ptr = UserPtr::from_ptr(name.as_ptr());
        task.sys_prctl(litebox_common_linux::PrctlArg::SetName(set_ptr))
            .expect("sys_prctl SetName failed");

        // Prepare buffer for prctl(PR_GET_NAME, get_buf)
        let mut get_buf = [0u8; litebox_common_linux::TASK_COMM_LEN];
        let get_ptr = UserPtrMut::from_ptr(get_buf.as_mut_ptr());

        task.sys_prctl(litebox_common_linux::PrctlArg::GetName(get_ptr))
            .expect("sys_prctl GetName failed");
        assert_eq!(
            &get_buf[..name.len()],
            name,
            "prctl get_name returned unexpected comm"
        );

        // Test too long name
        let long_name = [b'a'; litebox_common_linux::TASK_COMM_LEN + 10];
        let long_name_ptr = UserPtr::from_ptr(long_name.as_ptr());
        task.sys_prctl(litebox_common_linux::PrctlArg::SetName(long_name_ptr))
            .expect("sys_prctl SetName failed");

        // Get the name again
        let mut get_buf = [0u8; litebox_common_linux::TASK_COMM_LEN];
        let get_ptr = UserPtrMut::from_ptr(get_buf.as_mut_ptr());
        task.sys_prctl(litebox_common_linux::PrctlArg::GetName(get_ptr))
            .expect("sys_prctl GetName failed");
        assert_eq!(
            get_buf[litebox_common_linux::TASK_COMM_LEN - 1],
            0,
            "prctl get_name did not null-terminate the comm"
        );
        assert_eq!(
            &get_buf[..litebox_common_linux::TASK_COMM_LEN - 1],
            &long_name[..litebox_common_linux::TASK_COMM_LEN - 1],
            "prctl get_name returned unexpected comm for too long name"
        );
    }

    /// Installing a custom handler for SIGINT: a background OS thread sends
    /// a real SIGINT via `libc::kill`, which should interrupt a blocking sleep
    /// with `EINTR`.
    /// Target Linux only because it use tgkill syscall to send signal to specific thread.
    #[cfg(all(target_os = "linux", debug_assertions))]
    #[test]
    fn test_sigint_with_custom_handler() {
        use litebox_common_linux::signal::{SaFlags, SigAction, SigSet, Signal};
        use litebox_common_linux::{ClockId, TimerFlags, Timespec};

        let callback_addr = 0x1000usize; // dummy non-null address for the callback
        let task = crate::syscalls::tests::init_platform(None);
        <crate::syscalls::tests::TestPlatform as litebox::platform::ThreadProvider>::run_test_thread(|| {
            let act = SigAction {
                sigaction: callback_addr,
                flags: SaFlags::RESTORER,
                #[cfg(target_pointer_width = "64")]
                __pad: 0,
                restorer: 0,
                mask: SigSet::empty(),
            };
            let act_ptr = UserPtr::from_ptr(&raw const act);
            task.sys_rt_sigaction(
                Signal::SIGINT,
                Some(act_ptr),
                None,
                core::mem::size_of::<SigSet>(),
            )
            .expect("rt_sigaction failed");

            // Spawn a plain OS thread that sends a real SIGINT to this
            // specific thread after a short delay, giving it time to enter nanosleep.
            let pid = unsafe { libc::getpid() };
            let tid = unsafe { libc::syscall(libc::SYS_gettid) };
            let handle = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(200));
                // Safety: sending a signal to a thread in our own process is always valid.
                let ret = unsafe { libc::syscall(libc::SYS_tgkill, pid, tid, libc::SIGINT) };
                assert_eq!(ret, 0, "tgkill failed");
            });

            let mut request = Timespec {
                tv_sec: 10,
                tv_nsec: 0,
            };
            let result = task.sys_clock_nanosleep(
                ClockId::Monotonic,
                TimerFlags::empty(),
                litebox_common_linux::TimeParam::Timespec64(UserPtrMut::from_ptr(
                    &raw mut request,
                )),
                litebox_common_linux::TimeParam::None,
            );
            assert_eq!(
                result,
                Err(litebox_common_linux::errno::Errno::EINTR),
                "nanosleep should be interrupted by SIGINT from background thread"
            );

             // `process_signals` is called when about to switch back to userspace, so simulate that here.
             let mut stack = [0u8; 4096];
             #[cfg(target_arch = "x86_64")]
             let mut regs = litebox_common_linux::PtRegs { rsp: stack.as_mut_ptr() as usize + stack.len(), ..Default::default() };
             task.process_signals(&mut regs);
            assert_eq!(
                regs.get_ip(), callback_addr,
                "after processing signals, execution should be redirected to the custom handler"
            );

            handle.join().expect("background thread panicked");
        });
    }

    /// After the alarm deadline passes, a blocking operation should be
    /// interrupted and SIGALRM should be pending.
    #[test]
    fn test_alarm_fires_after_deadline() {
        use litebox::platform::{Instant as _, TimeProvider};
        use litebox_common_linux::{ClockId, TimerFlags, Timespec};

        let task = crate::syscalls::tests::init_platform(None);
        <crate::syscalls::tests::TestPlatform as litebox::platform::ThreadProvider>::run_test_thread(|| {
            let platform = task.global.platform;

            // Set a 1-second alarm.
            assert_eq!(task.sys_alarm(1).unwrap(), 0);

            let start = platform.now();

            // Block in a nanosleep longer than the alarm
            let mut remain = Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            let mut request = Timespec {
                tv_sec: 3,
                tv_nsec: 0,
            };
            let result = task.sys_clock_nanosleep(
                ClockId::Monotonic,
                TimerFlags::empty(),
                litebox_common_linux::TimeParam::Timespec64(UserPtrMut::from_ptr(&raw mut request)),
                litebox_common_linux::TimeParam::Timespec64(UserPtrMut::from_ptr(&raw mut remain)),
            );

            let elapsed = platform.now().duration_since(&start);

            // The nanosleep should have been interrupted by SIGALRM.
            assert_eq!(
                result,
                Err(litebox_common_linux::errno::Errno::EINTR),
                "nanosleep should have been interrupted"
            );
            let millis = remain.tv_sec.cast_unsigned() * 1000 + remain.tv_nsec / 1_000_000;
            // Allow tolerance for timer imprecision (especially on Windows).
            assert!(
                (1900..=2100).contains(&millis),
                "expected ~2s remaining, got {millis:?}"
            );

            let elapsed_ms = elapsed.as_millis();
            std::println!("Alarm fired after {elapsed_ms} ms");
            assert!(
                (900..=1100).contains(&elapsed_ms),
                "expected alarm after ~1000 ms, got {elapsed_ms} ms"
            );

            // The alarm should be consumed (deadline cleared).
            let remaining = task.sys_alarm(0).unwrap();
            assert_eq!(remaining, 0, "alarm should have been cleared by check");
        });
    }

    /// Cancelling an alarm before it fires should prevent signal delivery
    /// even if a blocking operation runs past the original deadline.
    #[test]
    fn test_alarm_cancel_prevents_signal() {
        use litebox_common_linux::{ClockId, TimerFlags, Timespec};

        let task = crate::syscalls::tests::init_platform(None);
        <crate::syscalls::tests::TestPlatform as litebox::platform::ThreadProvider>::run_test_thread(|| {
            assert_eq!(task.sys_alarm(1).unwrap(), 0);
            // Cancel before it fires.
            let remaining = task.sys_alarm(0).unwrap();
            assert!(remaining >= 1, "alarm should still have had time remaining");

            // A short nanosleep past the original deadline should complete
            // normally — no signal should interrupt it.
            let mut request = Timespec {
                tv_sec: 2,
                tv_nsec: 0,
            };
            let result = task.sys_clock_nanosleep(
                ClockId::Monotonic,
                TimerFlags::empty(),
                litebox_common_linux::TimeParam::Timespec64(UserPtrMut::from_ptr(&raw mut request)),
                litebox_common_linux::TimeParam::None,
            );
            assert_eq!(result, Ok(()), "nanosleep should not have been interrupted");

            assert!(
                !task.has_pending_signals(),
                "cancelled alarm should not produce SIGALRM"
            );
        });
    }

    #[test]
    fn test_pause_wakes_on_pending_signal() {
        use litebox_common_linux::{
            PtRegs,
            errno::Errno,
            signal::{SigSet, SigmaskHow, Signal},
        };

        let task = crate::syscalls::tests::init_platform(None);
        <crate::syscalls::tests::TestPlatform as litebox::platform::ThreadProvider>::run_test_thread(|| {
            let block_set = SigSet::empty().with(Signal::SIGUSR1);
            task.sys_rt_sigprocmask(
                SigmaskHow::SIG_BLOCK,
                Some(UserPtr::from_ptr(&raw const block_set)),
                None,
                core::mem::size_of::<SigSet>(),
            )
            .expect("block SIGUSR1 failed");

            assert_eq!(task.sys_alarm(1).unwrap(), 0);
            task.sys_tkill(task.tid, Signal::SIGUSR1.as_i32())
                .expect("tkill failed");
            assert!(!task.has_pending_signals(), "blocked SIGUSR1 should not be deliverable");

            let mut regs = PtRegs::default();
            task.process_signals(&mut regs);
            assert!(!task.has_pending_signals(), "blocked SIGUSR1 should remain undeliverable");

            task.sys_rt_sigprocmask(
                SigmaskHow::SIG_UNBLOCK,
                Some(UserPtr::from_ptr(&raw const block_set)),
                None,
                core::mem::size_of::<SigSet>(),
            )
            .expect("unblock SIGUSR1 failed");

            assert_eq!(task.sys_pause(), Err(Errno::EINTR));
            task.sys_alarm(0).unwrap();

            let pending = task.pending_signal_set();
            assert!(pending.contains(Signal::SIGUSR1), "expected SIGUSR1 pending");
            assert!(
                !pending.contains(Signal::SIGALRM),
                "SIGALRM must not be what woke pause()"
            );
        });
    }

    /// Setting alarm with SIG_IGN for SIGALRM: a blocking operation is still
    /// interrupted, but `process_signals` discards the signal.
    #[test]
    fn test_alarm_with_sigign() {
        use litebox_common_linux::signal::{SIG_IGN, SaFlags, SigAction, SigSet, Signal};
        use litebox_common_linux::{ClockId, TimerFlags, Timespec};

        let task = crate::syscalls::tests::init_platform(None);
        <crate::syscalls::tests::TestPlatform as litebox::platform::ThreadProvider>::run_test_thread(|| {
            // Install SIG_IGN for SIGALRM.
            let act = SigAction {
                sigaction: SIG_IGN,
                flags: SaFlags::empty(),
                #[cfg(target_pointer_width = "64")]
                __pad: 0,
                restorer: 0,
                mask: SigSet::empty(),
            };
            let act_ptr = UserPtr::from_ptr(&raw const act);
            task.sys_rt_sigaction(
                Signal::SIGALRM,
                Some(act_ptr),
                None,
                core::mem::size_of::<SigSet>(),
            )
            .expect("rt_sigaction failed");

            // Set a 1-second alarm and block in a short nanosleep.
            assert_eq!(task.sys_alarm(1).unwrap(), 0);
            let mut request = Timespec {
                tv_sec: 3,
                tv_nsec: 0,
            };
            let result = task.sys_clock_nanosleep(
                ClockId::Monotonic,
                TimerFlags::empty(),
                litebox_common_linux::TimeParam::Timespec64(UserPtrMut::from_ptr(&raw mut request)),
                litebox_common_linux::TimeParam::None,
            );

            // With SIG_IGN, nanosleep should NOT be interrupted — matching real
            // Linux behaviour where ignored signals are silently dropped at
            // send time and never make blocking syscalls return EINTR.
            assert_eq!(
                result,
                Ok(()),
                "nanosleep should complete normally when SIGALRM is ignored"
            );

            // No pending signals because the ignored SIGALRM was silently dropped.
            assert!(
                !task.has_pending_signals(),
                "SIG_IGN should cause SIGALRM to be silently dropped"
            );
        });
    }

    #[test]
    fn test_timer_delivers_correct_signal() {
        use litebox::platform::{TimerHandle as _, TimerProvider as _};
        use litebox_common_linux::signal::Signal;
        use litebox_common_linux::{ClockId, TimerFlags, Timespec};

        let task = crate::syscalls::tests::init_platform(None);
        <crate::syscalls::tests::TestPlatform as litebox::platform::ThreadProvider>::run_test_thread(|| {
            let platform = task.global.platform;

            // Create a timer that requests SIGUSR1
            let handle = platform
                .create_timer(Signal::SIGUSR1)
                .expect("create_timer failed");
            handle.set_timer(core::time::Duration::from_secs(1));

            // Block in a nanosleep longer than the timer.
            let mut request = Timespec {
                tv_sec: 5,
                tv_nsec: 0,
            };
            let result = task.sys_clock_nanosleep(
                ClockId::Monotonic,
                TimerFlags::empty(),
                litebox_common_linux::TimeParam::Timespec64(UserPtrMut::from_ptr(
                    &raw mut request,
                )),
                litebox_common_linux::TimeParam::None,
            );
            // The nanosleep should have been interrupted.
            assert_eq!(
                result,
                Err(litebox_common_linux::errno::Errno::EINTR),
                "nanosleep should be interrupted by the timer"
            );

            // Verify that SIGUSR1 (not SIGALRM) is the pending signal.
            let pending = task.pending_signal_set();
            assert!(
                pending.contains(Signal::SIGUSR1),
                "expected SIGUSR1 pending"
            );
            assert!(
                !pending.contains(Signal::SIGALRM),
                "SIGALRM should NOT be pending — the timer should have delivered SIGUSR1 instead"
            );

            // Clean up the timer.
            handle.delete_timer();
        });
    }

    /// Regression test for a multi-threaded `exit_group` deadlock: a background thread genuinely
    /// blocked in `FUTEX_WAIT` must actually be woken and get a chance to detach from the process
    /// once another thread calls `exit_group` -- mirroring the real hang this test was written to
    /// catch (`node -e "console.log(1)"` never terminating: several `CLONE_THREAD` background
    /// threads block in real waits, and the whole process only terminates once every one of them
    /// has detached, tracked by `Process::wait_for_exit`'s `nr_threads` count reaching zero).
    ///
    /// Also exercises the specific fix in `exit_group`/`kill_other_threads`: `ThreadRemote::interrupt`
    /// (`litebox::event::wait::ThreadHandle::interrupt`) can suspend another live OS thread
    /// directly (`SuspendThread` on Windows) -- an operation with no notion of "don't suspend while
    /// a Rust-level lock is held" by the *caller*. Before the fix, `exit_group` called `interrupt()`
    /// on every other thread while still holding `Process::inner`'s lock, needlessly widening the
    /// window during which any other thread contending for that same lock (e.g. another thread
    /// racing through its own exit, or a concurrent `clone()`) is blocked behind the whole
    /// interrupt loop. The fix collects the thread list and releases the lock before calling
    /// `interrupt()` on each. This test does not by itself reproduce that specific contention (it
    /// would require a precise race), but it does exercise the exact interrupt-and-wait-for-exit
    /// path end-to-end, with a bounded watchdog so a real regression fails the test instead of
    /// hanging CI forever.
    #[test]
    fn test_exit_group_wakes_thread_blocked_in_futex_wait() {
        let task = crate::syscalls::tests::init_platform(None);
        let process = task.process().clone();
        <crate::syscalls::tests::TestPlatform as litebox::platform::ThreadProvider>::run_test_thread(|| {
            // A futex word that will never reach any value other than 0 -- the background
            // thread's `FUTEX_WAIT` only returns if genuinely woken (by an explicit `FUTEX_WAKE`,
            // which nothing here issues, or by `exit_group`'s interrupt).
            let mut futex_word: u32 = 0;
            let futex_addr = (&raw mut futex_word) as usize;

            let bg = task.spawn_clone_for_test(move |bg_task| {
                // Register this OS thread's `ThreadHandle` with the background `Task`, exactly as
                // the platform does via `EnterShim::init`/`Task::handle_init_request` before a
                // real guest thread ever runs guest code -- without this, `ThreadRemote::interrupt`
                // (which reads `ThreadRemote::handle`, only ever populated by
                // `handle_init_request`) is a guaranteed no-op for this thread, since nothing else
                // ever populates it. `run_test_thread` sets up this OS thread's TLS/`ThreadHandle`
                // machinery at the platform level; `bg_task.set_thread_handle_for_test()` then
                // publishes it into `ThreadRemote::handle` the same way `handle_init_request` does.
                <crate::syscalls::tests::TestPlatform as litebox::platform::ThreadProvider>::run_test_thread(|| {
                    bg_task.set_thread_handle_for_test();

                    let futex_ptr = UserPtrMut::from_usize(futex_addr);
                    let result = bg_task.sys_futex(litebox_common_linux::FutexArgs::Wait {
                        addr: futex_ptr,
                        flags: litebox_common_linux::FutexFlags::PRIVATE,
                        val: 0,
                        timeout: litebox_common_linux::TimeParam::None,
                    });
                    // Interrupted by `exit_group`'s `is_exiting`-driven wake, not a real
                    // wake/timeout.
                    assert_eq!(
                        result,
                        Err(litebox_common_linux::errno::Errno::EINTR),
                        "background thread's FUTEX_WAIT should be interrupted by exit_group"
                    );
                    // Dropping `bg_task` here (falling off the end of this closure) runs
                    // `Task::prepare_for_exit` via `Drop for Task`, exactly as a real guest
                    // thread's `Task` drops once its syscall loop observes `is_exiting()` and
                    // returns `ContinueOperation::Terminate` -- decrementing `nr_threads`.
                });
            });

            // Give the background thread a real chance to enter the blocking wait before this
            // thread calls `exit_group` -- otherwise the test would trivially pass even with the
            // pre-fix code, since `is_exiting()`'s pre-block check in `commit_wait` would already
            // catch it before it ever blocks.
            std::thread::sleep(core::time::Duration::from_millis(50));

            task.exit_group(super::ExitStatus::Exit(0));

            bg.join().expect("background thread panicked");

            // Drop this (the calling) thread's own `Task`, matching a real `exit_group` caller,
            // which unwinds back out of the shim and drops its own `Task` immediately afterward
            // (see `Task::sys_exit_group`'s doc comment) -- `Process::wait_for_exit` waits for
            // EVERY thread, including this one, to detach, not just the background thread.
            drop(task);
        });

        // `Process::wait_for_exit` blocks until every thread in the process (both the background
        // thread above and the caller of `exit_group` itself) has detached. If `exit_group`'s
        // interrupt delivery regresses -- e.g. the background thread's `FUTEX_WAIT` is never
        // actually woken -- this hangs forever, so it must run on a watchdog-timed thread rather
        // than the test thread itself, to fail cleanly instead of hanging the whole test binary.
        let waiter = std::thread::spawn(move || {
            process.wait_for_exit();
        });

        let start = std::time::Instant::now();
        let timeout = core::time::Duration::from_secs(10);
        while !waiter.is_finished() {
            assert!(
                start.elapsed() < timeout,
                "Process::wait_for_exit did not return within {timeout:?} -- \
                 exit_group failed to wake the background thread blocked in FUTEX_WAIT"
            );
            std::thread::sleep(core::time::Duration::from_millis(10));
        }
        waiter.join().expect("wait_for_exit thread panicked");
    }

    /// Regression test for the real, reproduced `node -e "console.log(1)"` intermittent hang:
    /// a thread that dies while still recorded as the owner of a robust futex must have that
    /// futex's word updated (`FUTEX_OWNER_DIED`) and any waiter woken -- mirroring Linux's
    /// `exit_robust_list`/`handle_futex_death` (`kernel/futex/core.c`). Before this fix,
    /// `handle_futex_death` was `todo!()`, so processing any non-empty robust list at thread exit
    /// panicked instead of notifying waiters, permanently stranding a sibling thread blocked in
    /// `FUTEX_WAIT` on that lock (observed live via `LITEBOX_LOG=litebox_shim_linux=trace`: the
    /// main thread's `FUTEX_WAIT` on a glibc/musl "locked, has waiters" futex word (value `2`)
    /// with no matching `FUTEX_WAKE` ever logged again).
    ///
    /// This drives `Task::handle_futex_death` directly (rather than round-tripping through a
    /// hand-built `RobustListHead`/`RobustList` guest-memory layout, which is real guest-ABI
    /// plumbing already covered by `wake_robust_list`'s straightforward list-walking logic) to
    /// isolate exactly the piece that was unimplemented: does processing one owned, waited-on
    /// futex entry correctly mark it dead and wake the waiter, without panicking.
    #[test]
    fn test_handle_futex_death_wakes_waiter_and_sets_owner_died() {
        use core::sync::atomic::{AtomicU32, Ordering};

        let task = crate::syscalls::tests::init_platform(None);
        <crate::syscalls::tests::TestPlatform as litebox::platform::ThreadProvider>::run_test_thread(|| {
            let futex_word = alloc::boxed::Box::new(AtomicU32::new(0));
            let futex_addr = alloc::boxed::Box::into_raw(futex_word) as usize;

            let bg = task.spawn_clone_for_test(move |bg_task| {
                <crate::syscalls::tests::TestPlatform as litebox::platform::ThreadProvider>::run_test_thread(|| {
                    bg_task.set_thread_handle_for_test();

                    // Simulate this thread having locked a robust mutex: the futex word records
                    // this thread as owner, with the waiters bit set since the main thread is
                    // about to block on it.
                    #[expect(clippy::cast_sign_loss, reason = "tid is always non-negative")]
                    let owner_word = (bg_task.tid as u32) | super::FUTEX_WAITERS;
                    let futex_atomic = unsafe { &*(futex_addr as *const AtomicU32) };
                    futex_atomic.store(owner_word, Ordering::SeqCst);

                    // Wait for the main thread to actually be parked in FUTEX_WAIT before
                    // "dying" -- otherwise this would trivially pass even with the pre-fix
                    // `todo!()` never running (there'd be nothing parked to prove got woken).
                    std::thread::sleep(core::time::Duration::from_millis(50));

                    // The background thread now processes its (simulated) robust-list death
                    // notification directly, exactly as `prepare_for_exit`/`wake_robust_list`
                    // would for a real dying thread with this futex linked into its robust list
                    // -- without ever unlocking the futex itself.
                    bg_task
                        .handle_futex_death(UserPtr::from_usize(futex_addr), false)
                        .expect("handle_futex_death should not error for a well-formed entry");
                });
            });

            let futex_ptr = UserPtrMut::from_usize(futex_addr);
            let owner_word = {
                let futex_atomic = unsafe { &*(futex_addr as *const AtomicU32) };
                futex_atomic.load(Ordering::SeqCst)
            };
            let result = task.sys_futex(litebox_common_linux::FutexArgs::Wait {
                addr: futex_ptr,
                flags: litebox_common_linux::FutexFlags::PRIVATE,
                val: owner_word,
                timeout: litebox_common_linux::TimeParam::None,
            });
            assert_eq!(
                result,
                Ok(0),
                "main thread's FUTEX_WAIT on the robust futex should be woken once \
                 handle_futex_death runs for its dying owner, not hang forever"
            );

            let final_word = {
                let futex_atomic = unsafe { &*(futex_addr as *const AtomicU32) };
                futex_atomic.load(Ordering::SeqCst)
            };
            assert_eq!(
                final_word & super::FUTEX_OWNER_DIED,
                super::FUTEX_OWNER_DIED,
                "the futex word should have FUTEX_OWNER_DIED set once its owner dies without \
                 unlocking"
            );

            bg.join().expect("background thread panicked");
        });
    }

    #[test]
    fn test_parse_shebang_basic() {
        use super::parse_shebang;

        // Basic interpreter only
        assert_eq!(
            parse_shebang(b"#!/bin/bash\necho hello\n"),
            Some(("/bin/bash", None))
        );

        // Interpreter with single argument
        assert_eq!(
            parse_shebang(b"#!/usr/bin/env python3\nimport sys\n"),
            Some(("/usr/bin/env", Some("python3")))
        );

        // Leading spaces after #!
        assert_eq!(parse_shebang(b"#!  /bin/sh\n"), Some(("/bin/sh", None)));

        // Trailing spaces
        assert_eq!(parse_shebang(b"#!/bin/sh  \n"), Some(("/bin/sh", None)));

        // Argument with extra whitespace
        assert_eq!(
            parse_shebang(b"#!/usr/bin/env  -S python3\n"),
            Some(("/usr/bin/env", Some("-S python3")))
        );

        // No newline (truncated line — still valid)
        assert_eq!(parse_shebang(b"#!/bin/bash"), Some(("/bin/bash", None)));

        // Not a shebang
        assert_eq!(parse_shebang(b"\x7fELF"), None);

        // Empty after #!
        assert_eq!(parse_shebang(b"#!\n"), None);

        // Too short
        assert_eq!(parse_shebang(b"#"), None);
        assert_eq!(parse_shebang(b""), None);

        // Tab separator
        assert_eq!(
            parse_shebang(b"#!/usr/bin/env\tpython3\n"),
            Some(("/usr/bin/env", Some("python3")))
        );
    }

    #[test]
    fn prlimit_for_own_pid_succeeds_but_remote_pid_returns_esrch() {
        // Regression test: prlimit64(pid, ...) used to unconditionally panic (unimplemented!())
        // whenever pid != 0, even though pid == the caller's own real pid means exactly the same
        // thing per prlimit(2) -- and is what the util-linux `prlimit` CLI actually passes by
        // default (unlike getrlimit()/setrlimit(), which always use pid 0).
        use crate::syscalls::tests::init_platform;
        use litebox_common_linux::RlimitResource;

        let task = init_platform(None);
        let own_pid = task.sys_getpid();

        task.sys_prlimit(0, RlimitResource::NOFILE, None, None)
            .expect("pid=0 must mean self");
        task.sys_prlimit(own_pid, RlimitResource::NOFILE, None, None)
            .expect("pid == own real pid must mean self, not panic");

        let remote_pid = own_pid.wrapping_add(1234);
        let err = task
            .sys_prlimit(remote_pid, RlimitResource::NOFILE, None, None)
            .unwrap_err();
        assert_eq!(err, Errno::ESRCH);
    }
}
