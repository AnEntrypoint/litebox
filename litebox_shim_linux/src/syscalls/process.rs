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
}

// TODO: remove once we figure out how to handle Send/Sync for raw pointers.
unsafe impl<Platform: ShimPlatform> Send for ThreadState<Platform> {}

impl<Platform: ShimPlatform> ThreadState<Platform> {
    pub fn new_process(
        pid: i32,
        pm: litebox::mm::PageManager<Platform, { litebox::mm::linux::PAGE_SIZE }>,
        vforked: bool,
        parent: Option<Weak<Process<Platform>>>,
    ) -> Self {
        let remote = Arc::new(ThreadRemote::new());
        Self {
            init_state: Cell::new(ThreadInitState::None),
            process: Arc::new(Process::new(pid, remote.clone(), pm, vforked, parent)),
            remote,
            attached_tid: Cell::new(Some(pid)),
            clear_child_tid: Cell::new(None),
            robust_list: Cell::new(None),
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
        })
    }

    /// Detaches this thread from its process, returning `true` if this was the process's last
    /// thread (i.e. the process has now fully exited). Returns `false` if this thread was already
    /// detached (double-detach is possible via `ThreadState`'s own `Drop` running after
    /// `Task::prepare_for_exit` already called this explicitly) or was never attached.
    fn detach_from_process(&self) -> bool {
        if let Some(tid) = self.attached_tid.take() {
            self.process.detach_thread(tid)
        } else {
            false
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
    /// `1` from process creation until this (vforked) process's initial thread either calls
    /// `execve` successfully or exits, `0` otherwise. Only meaningful for a process created via
    /// `vfork()`; a plain `fork()`ed process's `vfork_done` is set immediately and never blocks
    /// anyone. `vfork()`'s POSIX contract requires the calling (parent) thread to be suspended
    /// for exactly this window -- see `do_clone`'s use of this field.
    vfork_done: <Platform as litebox::platform::RawMutexProvider>::RawMutex,
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
            vfork_done,
        }
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

    /// Detaches a thread from this process.
    ///
    /// Returns `true` if this was the process's last thread (i.e. the whole process has now
    /// exited) -- the caller (`Task::prepare_for_exit`) uses this to trigger reparenting of any
    /// still-running children this process leaves behind (see that function's doc comment).
    ///
    /// # Panics
    /// Panics if the thread ID does not exist in this process.
    fn detach_thread(&self, tid: i32) -> bool {
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
        if notify {
            self.nr_threads.wake_all();
        }
        process_exited
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
/// a memory-scanning false positive. Do not assume this comment's narrower margin is sufficient
/// mitigation without re-verifying against a live repro first.
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
        let process_exited = self.thread.detach_from_process();
        litebox_util_log::debug!(tid:% = self.tid, process_exited:% = process_exited; "prepare_for_exit: detach_from_process done");
        if process_exited {
            // Real Linux implicitly closes every fd a process holds when its last thread exits,
            // releasing each open file description's reference so peers (e.g. a pipe's reader,
            // waiting for EOF once the last writer goes away) are correctly notified. This
            // shim's fd bookkeeping does not do that automatically -- see
            // `close_all_fds_on_process_exit`'s doc comment for the real, reproduced hang this
            // fixes (a pipe reader blocking forever because a writer's fd was never released on
            // the writer's ordinary process exit). Must happen before reparenting orphans below,
            // though the ordering is not itself load-bearing -- fd closure and child reparenting
            // are independent cleanup steps.
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

        // Unlike the blocking path, a `WNOHANG` poll must NOT remove the child from our children
        // list unless it has actually already exited -- otherwise a later real wait for that
        // same child would incorrectly see `ECHILD`.
        let (child_pid, child_process) = {
            let children = process.children.lock();
            let idx = if pid > 0 {
                children.iter().position(|(p, _)| *p == pid)
            } else if pid == -1 {
                children.first().map(|_| 0)
            } else {
                // Waiting for a specific process group (pid == 0 or pid < -1) is not
                // supported yet -- every child we create is in its own group today anyway.
                log_unsupported!("wait4 with pid={pid} (process-group wait)");
                return Err(Errno::EINVAL);
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
                // Validate the user-controlled TLS base before spawning the thread.
                if !litebox_common_linux::arch::is_valid_user_fs_base(addr) {
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

        let (thread, init_state, pid, ppid) = if is_process_clone {
            // Real `fork()`/`vfork()`: build a brand-new `Process` (new thread group) whose
            // address space is an eager duplicate of the parent's -- writes made by either the
            // parent or the child after this point are independent.
            let (dest_pm, relocations) =
                unsafe { self.process().pm.duplicate(&self.global.litebox) }.map_err(|err| {
                    litebox_util_log::error!(err:% = err; "failed to duplicate address space for fork()");
                    Errno::ENOMEM
                })?;
            let vforked = flags.contains(CloneFlags::VFORK);
            let thread = crate::syscalls::process::ThreadState::new_process(
                child_tid,
                dest_pm,
                vforked,
                Some(Arc::downgrade(self.process())),
            );

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
                        signals: self.signals.clone_for_new_task(),
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
        seq_macro::seq!(N in 0..16 {
            let mut limits = [
                #(
                    AtomicRlimit::new(0, 0),
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
        Self { limits }
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
        let old_rlimit = match resource {
            litebox_common_linux::RlimitResource::NOFILE
            | litebox_common_linux::RlimitResource::STACK => {
                self.thread.process.limits.get_rlimit(resource)
            }
            _ => {
                log_unsupported!("Unsupported resource for get_rlimit: {:?}", resource);
                return Err(Errno::EINVAL);
            }
        };
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
            match resource {
                litebox_common_linux::RlimitResource::NOFILE => {
                    let new_max_fd = new_limit.rlim_cur.saturating_sub(1);
                    self.thread.process.limits.set_rlimit(resource, new_limit);
                    self.files.borrow().set_max_fd(new_max_fd);
                }
                _ => unimplemented!("Unsupported resource for set_rlimit: {:?}", resource),
            }
        }
        Ok(old_rlimit)
    }

    /// Handle syscall `prlimit64`.
    ///
    /// Note for now setting new limits is not supported yet, and thus returning constant values
    /// for the requested resource. Getting resources for a specific PID is also not supported yet.
    pub(crate) fn sys_prlimit(
        &self,
        pid: i32,
        resource: litebox_common_linux::RlimitResource,
        new_rlim: Option<UserPtr<litebox_common_linux::Rlimit64>>,
        old_rlim: Option<UserPtrMut<litebox_common_linux::Rlimit64>>,
    ) -> Result<(), Errno> {
        if pid != 0 {
            unimplemented!("prlimit for a specific PID is not supported yet");
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
            litebox_common_linux::ClockId::MonotonicCoarse => {
                // CLOCK_MONOTONIC_COARSE - provides faster but less precise monotonic time
                // For simplicity, we can reuse the same monotonic time as CLOCK_MONOTONIC
                // In a real implementation, this would typically have lower resolution
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
            | litebox_common_linux::ClockId::MonotonicCoarse => {
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
            litebox_common_linux::ClockId::MonotonicCoarse => {
                // Coarse clocks typically have lower resolution (e.g., 4 millisecond)
                Duration::from_millis(4)
            }
            litebox_common_linux::ClockId::RealTime | litebox_common_linux::ClockId::Monotonic => {
                // For most modern systems, the resolution is typically 1 nanosecond
                // This is a reasonable default for high-resolution timers
                Duration::from_nanos(1)
            }
            _ => unimplemented!(),
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
}
