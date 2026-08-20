// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Signal handling syscalls and support.

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "x86_64")]
mod x86_64;

#[cfg(target_arch = "aarch64")]
use aarch64 as arch;
use litebox_common_linux::signal::SignalDisposition;
#[cfg(target_arch = "x86_64")]
use x86_64 as arch;
use zerocopy::FromZeros;

use crate::syscalls::process::ExitStatus;
use crate::{ShimFS, ShimPlatform, Task, UserPtr, UserPtrMut};
use alloc::collections::vec_deque::VecDeque;
use alloc::sync::Arc;
use core::cell::{Cell, RefCell};
use litebox::{shim::Exception, sync::Mutex, utils::ReinterpretUnsignedExt as _};
use litebox_common_linux::signal::{
    MINSIGSTKSZ, NSIG, SI_KERNEL, SI_USER, SIG_DFL, SIG_IGN, SaFlags, SigAction, SigAltStack,
    SigSet, Siginfo, SiginfoData, SigmaskHow, Signal, SsFlags, Ucontext,
};
use litebox_common_linux::{PtRegs, errno::Errno};

pub(crate) struct SignalState<Platform: ShimPlatform> {
    /// Pending thread signals.
    pending: RefCell<PendingSignals>,
    /// Pending process signals (shared across all threads).
    shared_pending: Arc<Mutex<Platform, PendingSignals>>,
    /// Currently blocked signals.
    blocked: Cell<SigSet>,
    /// Signal handlers.
    handlers: RefCell<Arc<SignalHandlers<Platform>>>,
    /// Alternate signal stack.
    altstack: Cell<SigAltStack>,
    /// The last exception info recorded for signal delivery.
    last_exception: Cell<litebox::shim::ExceptionInfo>,
    /// Guest-visible address of a litebox-synthesized `rt_sigreturn` trampoline, lazily
    /// allocated on first use, or 0 if not yet allocated.
    sigreturn_trampoline: Cell<usize>,
}

impl<Platform: ShimPlatform> SignalState<Platform> {
    /// `shared_pending` must be the exact same `Arc` stored as the new process's
    /// `Process::shared_pending` (see that field's doc comment) -- this is what lets a signal
    /// pushed there from a *different* process's `do_kill` (targeting this one as a live child)
    /// actually be observed by this process's own `process_signals`/`has_pending_signals`, which
    /// only ever look at `self.shared_pending`, never at `Process` directly.
    pub fn new_process(shared_pending: Arc<Mutex<Platform, PendingSignals>>) -> Self {
        Self {
            pending: RefCell::new(PendingSignals::new()),
            shared_pending,
            blocked: Cell::new(SigSet::empty()),
            handlers: RefCell::new(Arc::new(SignalHandlers::new())),
            altstack: Cell::new(SigAltStack {
                sp: 0,
                flags: SsFlags::DISABLE,
                size: 0,
                __pad: 0,
            }),
            last_exception: Cell::new(litebox::shim::ExceptionInfo::default()),
            sigreturn_trampoline: Cell::new(0),
        }
    }

    /// Build the signal state for a new task from this one, via either `clone()` (a new
    /// *thread* of the same process, `new_process_shared_pending = None`) or `fork()`/`vfork()`
    /// (a genuine new *process*, `new_process_shared_pending = Some(the new Process's
    /// shared_pending Arc)` -- see [`Self::new_process`]'s doc comment on why it must be that
    /// exact `Arc`, not a freshly allocated one).
    ///
    /// Real Linux shares process-wide signal state (the pending-signal queue and handler
    /// dispositions) across threads of the same process (`CLONE_THREAD`/`CLONE_SIGHAND`) but
    /// gives a freshly `fork()`'d child its own independent copies -- later `sigaction()` calls
    /// or process-directed (`kill(pid, ...)`) signals in either the parent or the child must not
    /// affect the other. Before this distinction existed here, every call shared both
    /// unconditionally, so a signal meant for a forked child's process-wide queue could
    /// incorrectly land in (and be consumed by) the parent's queue instead, or vice versa, and a
    /// later `sigaction()` in either process would silently change the other's handler too.
    pub fn clone_for_new_task(
        &self,
        new_process_shared_pending: Option<Arc<Mutex<Platform, PendingSignals>>>,
    ) -> Self {
        let new_process = new_process_shared_pending.is_some();
        Self {
            // Reset pending
            pending: RefCell::new(PendingSignals::new()),
            shared_pending: new_process_shared_pending
                .unwrap_or_else(|| self.shared_pending.clone()),
            // Preserve blocked
            blocked: Cell::new(self.blocked.get()),
            handlers: if new_process {
                // Snapshot the parent's *current* dispositions into an independent copy (real
                // fork() semantics), rather than sharing the same handlers, or resetting to
                // SIG_DFL (which would incorrectly discard whatever the parent had configured).
                RefCell::new(Arc::new(SignalHandlers {
                    inner: Mutex::new(self.handlers.borrow().inner.lock().clone()),
                }))
            } else {
                self.handlers.clone()
            },
            // Clear altstack
            altstack: SigAltStack {
                flags: SsFlags::DISABLE,
                sp: 0,
                size: 0,
                __pad: 0,
            }
            .into(),
            // Preserve last exception
            last_exception: self.last_exception.clone(),
            // A clone()'d thread shares the SAME address space, so an already-allocated
            // trampoline address stays valid; fork()'s child gets a COW-copied address space
            // where the same virtual address is still valid too. Only execve() (reset_for_exec,
            // below) actually invalidates it.
            sigreturn_trampoline: Cell::new(self.sigreturn_trampoline.get()),
        }
    }

    /// Resets signal state for an `execve` call.
    pub(crate) fn reset_for_exec(&self) {
        // execve() replaces the entire address space with a fresh ELF image -- any previously
        // allocated trampoline address is no longer valid (or even mapped); `write_signal_frame`
        // lazily re-allocates a fresh one on next use.
        self.sigreturn_trampoline.set(0);
        let mut handlers = self.handlers.borrow_mut();
        // Ensure that the signal handlers are no longer shared.
        let handlers = Arc::make_mut(&mut handlers);
        // Reset the handlers to defaults.
        for handler in &mut handlers.inner.get_mut().handlers {
            handler.action = SigAction {
                sigaction: if handler.action.sigaction == SIG_IGN {
                    SIG_IGN
                } else {
                    SIG_DFL
                },
                restorer: 0,
                flags: SaFlags::empty(),
                mask: SigSet::empty(),
                __pad: 0,
            };
        }
        self.clear_sigaltstack();
    }
}

struct SignalHandlers<Platform: ShimPlatform> {
    inner: Mutex<Platform, SignalHandlersInner>,
}

#[derive(Clone)]
struct SignalHandlersInner {
    handlers: [Handler; NSIG],
}

impl SignalHandlersInner {
    /// Returns the array index for the given signal.
    fn sig_index(signal: Signal) -> usize {
        (signal.as_i32().reinterpret_as_unsigned() - 1) as usize
    }
}

impl core::ops::Index<Signal> for SignalHandlersInner {
    type Output = Handler;

    fn index(&self, signal: Signal) -> &Self::Output {
        &self.handlers[Self::sig_index(signal)]
    }
}

impl core::ops::IndexMut<Signal> for SignalHandlersInner {
    fn index_mut(&mut self, signal: Signal) -> &mut Self::Output {
        &mut self.handlers[Self::sig_index(signal)]
    }
}

#[derive(Clone)]
struct Handler {
    action: SigAction,
    /// The user cannot change this action.
    immutable: bool,
}

impl<Platform: ShimPlatform> SignalHandlers<Platform> {
    fn new() -> Self {
        Self {
            inner: Mutex::new(SignalHandlersInner {
                handlers: core::array::from_fn(|i| Handler {
                    action: SigAction {
                        sigaction: SIG_DFL,
                        restorer: 0,
                        flags: SaFlags::empty(),
                        mask: SigSet::empty(),
                        __pad: 0,
                    },
                    immutable: i == SignalHandlersInner::sig_index(Signal::SIGKILL)
                        || i == SignalHandlersInner::sig_index(Signal::SIGSTOP),
                }),
            }),
        }
    }
}

impl<Platform: ShimPlatform> Clone for SignalHandlers<Platform> {
    fn clone(&self) -> Self {
        Self {
            inner: Mutex::new(self.inner.lock().clone()),
        }
    }
}

/// Shared with [`super::process::Process`] (as `Process::shared_pending`) so a signal aimed at a
/// live, shim-known child process (see `do_kill`'s remote-child case) can be queued directly from
/// the sender's context, without needing the target's own `Task`/`SignalState` in scope.
pub(crate) struct PendingSignals {
    /// The set of pending signals.
    pending: SigSet,
    /// The queue of pending siginfo structures.
    queue: VecDeque<Siginfo>,
}

impl PendingSignals {
    pub(crate) fn new() -> Self {
        Self {
            pending: SigSet::empty(),
            queue: VecDeque::new(),
        }
    }

    fn next(&self, blocked: SigSet) -> Option<Signal> {
        const EXCEPTION_SIGNALS: SigSet = SigSet::empty()
            .with(Signal::SIGSEGV)
            .with(Signal::SIGBUS)
            .with(Signal::SIGFPE)
            .with(Signal::SIGILL)
            .with(Signal::SIGTRAP);

        let pending = self.pending & !blocked;

        // Look for exception signals first since these must be delivered with
        // the user context at the time of the exception.
        let next = (pending & EXCEPTION_SIGNALS)
            .lowest_set()
            .or_else(|| pending.lowest_set())?;

        Some(next)
    }

    fn remove(&mut self, signal: Signal) -> Siginfo {
        // Find the entry.
        let pos = self
            .queue
            .iter()
            .position(|info| info.signo == signal.as_i32())
            .expect("removing non-pending signal");

        // If there are no more entries with this signal number, remove it from
        // the pending mask.
        let more = self
            .queue
            .iter()
            .skip(pos + 1)
            .any(|info| info.signo == signal.as_i32());
        if !more {
            self.pending.remove(signal);
        }

        self.queue.remove(pos).unwrap()
    }

    pub(crate) fn push(
        &mut self,
        rlimits: &super::process::ResourceLimits,
        signal: Signal,
        siginfo: Siginfo,
    ) {
        assert_eq!(signal.as_i32(), siginfo.signo);

        // Don't queue duplicates for standard signals.
        if !signal.is_rt_signal() && self.pending.contains(signal) {
            return;
        }

        // Restrict maximum queued signals via rlimits when Linux would do so.
        if signal.is_rt_signal() || (siginfo.code != SI_USER && siginfo.code != SI_KERNEL) {
            let limit = rlimits.get_rlimit_cur(litebox_common_linux::RlimitResource::SIGPENDING);
            if self.queue.len() >= limit {
                // Drop the signal.
                return;
            }
        }
        self.queue.push_back(siginfo);
        self.pending.add(signal);
    }
}

/// Returns whether `sp` is within the given signal stack.
fn is_on_stack(stack: &SigAltStack, sp: usize) -> bool {
    if stack.flags.contains(SsFlags::DISABLE) {
        return false;
    }
    let stack_start = stack.sp;
    let stack_end = stack.sp + stack.size;
    sp >= stack_start && sp < stack_end
}

/// Creates a `Siginfo` for an exception signal.
fn siginfo_exception(signal: Signal, fault_address: usize) -> Siginfo {
    Siginfo {
        signo: signal.as_i32(),
        errno: 0,
        code: SI_KERNEL,
        __pad: 0,
        data: SiginfoData::new_addr(fault_address),
    }
}

/// Creates a `Siginfo` for a signal sent by a user process via `kill()`,
/// `tkill()`, or `tgkill()`.
pub(crate) fn siginfo_kill(signal: Signal) -> Siginfo {
    Siginfo {
        signo: signal.as_i32(),
        errno: 0,
        code: SI_USER,
        __pad: 0,
        data: SiginfoData::new_zeroed(),
    }
}

impl<Platform: ShimPlatform> SignalState<Platform> {
    /// Updates the blocked signal mask.
    fn set_signal_mask(&self, mask: SigSet) {
        self.blocked.set(mask);
    }

    /// Sets the alternate signal stack.
    fn set_sigaltstack(&self, ss: SigAltStack) -> Result<(), Errno> {
        if !ss
            .flags
            .difference(SsFlags::DISABLE | SsFlags::ONSTACK | SsFlags::AUTODISARM)
            .is_empty()
        {
            Err(Errno::EINVAL)
        } else if ss.flags.contains(SsFlags::DISABLE) {
            self.clear_sigaltstack();
            Ok(())
        } else if ss.sp.checked_add(ss.size).is_none() {
            Err(Errno::EINVAL)
        } else if ss.size < MINSIGSTKSZ {
            Err(Errno::ENOMEM)
        } else {
            self.altstack.set(SigAltStack {
                sp: ss.sp,
                flags: ss.flags & SsFlags::AUTODISARM,
                size: ss.size,
                __pad: 0,
            });
            Ok(())
        }
    }

    /// Clears the alternate signal stack.
    fn clear_sigaltstack(&self) {
        self.altstack.set(SigAltStack {
            sp: 0,
            flags: SsFlags::DISABLE,
            size: 0,
            __pad: 0,
        });
    }

    fn deliver_signal(
        &self,
        signal: Signal,
        siginfo: &Siginfo,
        action: &SigAction,
        ctx: &mut PtRegs,
        sigreturn_trampoline: usize,
    ) -> Result<(), DeliverFault> {
        let sp = arch::sp(ctx);
        let on_alt_stack = is_on_stack(&self.altstack.get(), sp);
        let altstack = self.altstack.get();
        let switch_stacks = action.flags.contains(SaFlags::ONSTACK)
            && !on_alt_stack
            && !altstack.flags.contains(SsFlags::DISABLE);
        let sp = if switch_stacks {
            altstack.sp + altstack.size
        } else {
            sp
        };

        let frame_addr = arch::get_signal_frame(sp, action);

        if (switch_stacks || on_alt_stack) && !is_on_stack(&altstack, frame_addr) {
            return Err(DeliverFault);
        }

        // Pass-29 diagnostic: the long-running `rip == 0` crash investigation (see FINDINGS.txt)
        // established the guest branches to address 0 under its own power. `write_signal_frame`
        // (x86_64.rs) sets `ctx.rip = action.sigaction` directly from caller-supplied dispositions
        // with no validation; `action.sigaction == 0` is supposed to be structurally impossible
        // here (the `SIG_DFL`/`SIG_IGN` match arms in `process_signals` are meant to intercept it
        // before `deliver_signal` is ever called), but has never been empirically confirmed never
        // to happen. Gated behind the `error!` log level (already filterable/cheap when disabled)
        // rather than an env var, since this crate is `no_std` and has no direct env access; this
        // is the cheapest possible falsification of the "signal delivery hands the guest a null
        // handler" hypothesis.
        if action.sigaction == 0 {
            litebox_util_log::error!(
                signal:? = signal;
                "[diag-rip0-sigdeliver] delivering signal with sigaction==0 (should be unreachable: SIG_DFL/SIG_IGN ought to have intercepted this in process_signals)"
            );
        }

        self.write_signal_frame(frame_addr, siginfo, action, ctx, sigreturn_trampoline)?;

        let mut mask = self.blocked.get() | action.mask;
        if !action.flags.contains(SaFlags::NODEFER) {
            mask.add(signal);
        }
        self.set_signal_mask(mask);

        if altstack.flags.contains(SsFlags::AUTODISARM) {
            self.clear_sigaltstack();
        }
        Ok(())
    }
}

/// A fault when delivering a signal.
struct DeliverFault;

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Returns the already-allocated sigreturn trampoline's guest address, or 0 if
    /// [`Task::ensure_sigreturn_trampoline`] has never been called for this address space.
    /// Used to recognize a trap at exactly this address as the trampoline's own `brk`, not a
    /// genuine guest breakpoint (see aarch64's `LinuxShimEntrypoints::exception`).
    #[cfg(target_arch = "aarch64")]
    pub(crate) fn sigreturn_trampoline_addr(&self) -> usize {
        self.signals.sigreturn_trampoline.get()
    }

    /// Returns the guest-visible address of a litebox-synthesized `rt_sigreturn` trampoline
    /// (a tiny `brk #0xdead` stub), allocating it via a real guest `mmap` on first
    /// use and caching the address for the lifetime of this address space (see
    /// `SignalState::sigreturn_trampoline`'s doc comment for why it's invalidated on `execve`
    /// but preserved across `clone`/`fork`). Returns 0 if the allocation itself fails (e.g. the
    /// guest is out of address space) -- the caller treats that the same as "no restorer
    /// available" (see `write_signal_frame`'s aarch64 doc comment).
    ///
    /// aarch64-only: x86_64 glibc always supplies its own real restorer transparently, so no
    /// synthesized one is ever needed there.
    ///
    /// Uses `brk #0xdead` (a debug breakpoint trap, SIGTRAP) rather than the real
    /// `mov x8, #139 ; svc #0` (139 = __NR_rt_sigreturn) an earlier version of this trampoline
    /// used: the real syscall number is unconditionally seccomp-allowed on this platform (the
    /// host's own real signal-handler returns implicitly issue the same real syscall, and
    /// seccomp has no way to distinguish that from this trampoline's own explicit call --
    /// trapping it unconditionally live-proved unfixable, see the seccomp allow-list's doc
    /// comment on `SYS_rt_sigreturn`), so a guest reaching the real syscall number here would
    /// silently bypass litebox's own signal-frame restoration and hit the real kernel's
    /// `rt_sigreturn` instead, corrupting this thread's actual execution state. `brk` traps
    /// unconditionally and unambiguously (host code never executes this specific immediate),
    /// routing cleanly to `exception_signal_handler`'s SIGTRAP dispatch instead.
    #[cfg(target_arch = "aarch64")]
    pub(crate) fn ensure_sigreturn_trampoline(&self) -> usize {
        // `brk #0xdead` -- see this function's doc comment. Encoded by hand (verified via
        // `as`/`objdump`) rather than depending on an assembler at build time for 4 fixed bytes.
        const TRAMPOLINE_CODE: [u8; 4] = [0xa0, 0xd5, 0x3b, 0xd4];
        let existing = self.signals.sigreturn_trampoline.get();
        if existing != 0 {
            return existing;
        }
        let Ok(page) = self.sys_mmap(
            0,
            litebox::mm::linux::PAGE_SIZE,
            litebox_common_linux::ProtFlags::PROT_READ_EXEC,
            litebox_common_linux::MapFlags::MAP_PRIVATE
                | litebox_common_linux::MapFlags::MAP_ANONYMOUS,
            -1,
            0,
        ) else {
            return 0;
        };
        let addr = page.as_usize();
        // The mapping above is already PROT_EXEC; briefly mprotect it writable to seed the
        // trampoline bytes, then drop write permission again -- keeps this guest-visible page
        // execute-only for its entire useful lifetime (W^X), matching how a real VDSO page
        // behaves, rather than leaving a permanently writable+executable page around.
        if self
            .sys_mprotect(
                page,
                litebox::mm::linux::PAGE_SIZE,
                litebox_common_linux::ProtFlags::PROT_READ_WRITE,
            )
            .is_err()
        {
            return 0;
        }
        let write_ok = UserPtrMut::<[u8; 4]>::from_usize(addr)
            .write_at_offset::<Platform>(0, TRAMPOLINE_CODE)
            .is_some();
        let _ = self.sys_mprotect(
            page,
            litebox::mm::linux::PAGE_SIZE,
            litebox_common_linux::ProtFlags::PROT_READ_EXEC,
        );
        if !write_ok {
            return 0;
        }
        self.signals.sigreturn_trampoline.set(addr);
        addr
    }

    pub(crate) fn with_temporary_signal_mask<R>(&self, mask: SigSet, f: impl FnOnce() -> R) -> R {
        let old = self.signals.blocked.get();
        self.signals.set_signal_mask(mask);
        let result = f();
        self.signals.set_signal_mask(old);
        result
    }

    pub(crate) fn sys_rt_sigprocmask(
        &self,
        how: SigmaskHow,
        set_ptr: Option<UserPtr<SigSet>>,
        oldset_ptr: Option<UserPtrMut<SigSet>>,
        sigsetsize: usize,
    ) -> Result<usize, Errno> {
        if sigsetsize != core::mem::size_of::<SigSet>() {
            return Err(Errno::EINVAL);
        }
        let set = if let Some(set_ptr) = set_ptr {
            Some(set_ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?)
        } else {
            None
        };

        if let Some(oldset_ptr) = oldset_ptr {
            let oldset = self.signals.blocked.get();
            oldset_ptr
                .write_at_offset::<Platform>(0, oldset)
                .ok_or(Errno::EFAULT)?;
        }

        if let Some(set) = set {
            let mut blocked = self.signals.blocked.get();
            match how {
                SigmaskHow::SIG_BLOCK => {
                    blocked = blocked | set;
                }
                SigmaskHow::SIG_UNBLOCK => {
                    blocked = blocked & !set;
                }
                SigmaskHow::SIG_SETMASK => {
                    blocked = set;
                }
            }
            self.signals.set_signal_mask(blocked);
        }

        Ok(0)
    }

    pub(crate) fn sys_sigaltstack(
        &self,
        ss_ptr: Option<UserPtr<SigAltStack>>,
        old_ss_ptr: Option<UserPtrMut<SigAltStack>>,
        ctx: &PtRegs,
    ) -> Result<usize, Errno> {
        let mut old_ss = self.signals.altstack.get();
        let is_on_stack = is_on_stack(&old_ss, arch::sp(ctx));
        if let Some(old_ss_ptr) = old_ss_ptr {
            if is_on_stack {
                old_ss.flags |= SsFlags::ONSTACK;
            }
            old_ss_ptr
                .write_at_offset::<Platform>(0, old_ss)
                .ok_or(Errno::EFAULT)?;
        }
        if let Some(ss_ptr) = ss_ptr {
            if is_on_stack {
                return Err(Errno::EPERM);
            }
            let ss = ss_ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
            self.signals.set_sigaltstack(ss)?;
        }
        Ok(0)
    }

    pub(crate) fn sys_rt_sigreturn(&self, ctx: &mut PtRegs) -> Result<usize, Errno> {
        let uctx_addr = arch::uctx_addr(ctx);
        let uctx_ptr = UserPtr::<Ucontext>::from_usize(uctx_addr);
        let Some(uctx) = uctx_ptr.read_at_offset::<Platform>(0) else {
            self.force_signal(Signal::SIGSEGV, false);
            return Err(Errno::EFAULT);
        };

        // Restore the alternate signal stack, ignoring errors.
        self.signals.set_sigaltstack(uctx.stack).ok();

        self.signals.set_signal_mask(uctx.sigmask);

        Ok(arch::restore_sigcontext(ctx, &uctx.mcontext))
    }

    pub(crate) fn sys_rt_sigaction(
        &self,
        signal: Signal,
        act_ptr: Option<UserPtr<SigAction>>,
        oldact_ptr: Option<UserPtrMut<SigAction>>,
        sigsetsize: usize,
    ) -> Result<usize, Errno> {
        if signal == Signal::SIGKILL || signal == Signal::SIGSTOP {
            return Err(Errno::EINVAL);
        }
        if sigsetsize != core::mem::size_of::<SigSet>() {
            return Err(Errno::EINVAL);
        }
        let act = if let Some(act_ptr) = act_ptr {
            Some(act_ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?)
        } else {
            None
        };

        let handlers = self.signals.handlers.borrow();
        let old_act = {
            let mut inner = handlers.inner.lock();
            let handler = &mut inner[signal];
            if handler.immutable {
                return Err(Errno::EINVAL);
            }
            let old_act = handler.action;
            if let Some(act) = act {
                handler.action = act;
            }
            old_act
        };

        if let Some(oldact_ptr) = oldact_ptr {
            oldact_ptr
                .write_at_offset::<Platform>(0, old_act)
                .ok_or(Errno::EFAULT)?;
        }

        Ok(0)
    }

    pub(crate) fn sys_kill(&self, pid: i32, signal: i32) -> Result<usize, Errno> {
        self.do_kill(Some(pid), None, signal)
    }

    pub(crate) fn sys_tkill(&self, tid: i32, signal: i32) -> Result<usize, Errno> {
        self.do_kill(None, Some(tid), signal)
    }

    pub(crate) fn sys_tgkill(&self, pid: i32, tid: i32, signal: i32) -> Result<usize, Errno> {
        self.do_kill(Some(pid), Some(tid), signal)
    }

    fn do_kill(&self, pid: Option<i32>, tid: Option<i32>, signal: i32) -> Result<usize, Errno> {
        let signal = Signal::try_from(signal)?;
        if tid.is_some_and(|tid| tid != self.tid) {
            log_unsupported!("sys_tkill/sys_tgkill with a remote tid");
            return Err(Errno::ESRCH);
        }
        // Process-directed delivery to a live child `Process`: any one of its threads may end up
        // handling it, exactly like a same-process `send_shared_signal`. Real Linux's SIG_IGN
        // check can't be done here (that reads the *target's* handler dispositions, which live
        // on its own `Task`, unreachable from a `Process` handle alone) -- `process_signals`
        // already discards an ignored signal correctly once the child wakes and looks at its own
        // handlers, so this only costs the child one spurious EINTR on an ignored signal, never
        // an incorrect delivery.
        let deliver_to_child = |child: &super::process::Process<Platform>| {
            child
                .shared_pending
                .lock()
                .push(&child.limits, signal, siginfo_kill(signal));
            child.interrupt_all_threads();
        };

        // `pid == 0` (send to the caller's own process group), a negative `pid` (send to process
        // group `-pid`), and `pid == -1` (send to every process the caller may signal) are
        // approximated as "signal self plus any live child currently in that same group": this
        // shim has no registry of *arbitrary* other live processes to enumerate a process group
        // or the whole guest (see `sys_setpgid`'s doc comment on why sessions/cross-process pid
        // lookups aren't modeled at all), but a live child that's been moved into the group via
        // `setpgid()` -- the standard shell-job-control/process-supervisor pattern of putting a
        // whole spawned pipeline into one group -- *is* reachable via `children`, covering
        // "kill the whole pipeline"/"kill the whole group" without needing a general registry.
        let self_pgid = self.sys_getpgid(0)?;
        let targets_self = match pid {
            None | Some(0 | -1) => true,
            Some(p) if p == self.pid => true,
            Some(p) => p.checked_neg().is_some_and(|group| group == self_pgid),
        };
        let mut delivered = targets_self;
        if targets_self {
            self.send_signal(signal, siginfo_kill(signal));
        }
        // `tid.is_some()` (tkill/tgkill) always targets one specific thread and never carries
        // group semantics -- only plain `kill(pid, sig)` (tid.is_none()) does.
        let target_group = tid
            .is_none()
            .then_some(pid)
            .flatten()
            .and_then(|p| match p {
                0 | -1 => Some(self_pgid),
                p if p < 0 => p.checked_neg(),
                _ => None,
            });
        if let Some(group) = target_group {
            for child in &self.process().children_in_group(group) {
                deliver_to_child(child);
                delivered = true;
            }
            // A group op targeting neither self's own group nor any reachable child's group has
            // literally nothing this shim can deliver to -- ESRCH, matching real Linux's
            // behavior for a pgid with zero members, rather than silently reporting success.
            return if delivered { Ok(0) } else { Err(Errno::ESRCH) };
        }
        if targets_self {
            return Ok(0);
        }
        // A genuine remote pid (some other, specific process): still no shim-wide pid registry to
        // find an arbitrary process by pid, but a *direct child* of the caller is reachable via
        // `children` (populated by `do_clone`'s process-clone branch) -- covering the single most
        // common real-world case, a supervisor/process-manager signaling a worker it spawned.
        if let Some(pid) = pid
            && pid > 0
            && let Some(child) = self.process().find_child(pid)
        {
            deliver_to_child(&child);
            return Ok(0);
        }
        // Pass 141, documented gap: a cross-process `fork()` child (`LITEBOX_PROCESS_FORK=1`) is
        // tracked in `Process::cross_process_children`, not `children` -- `find_child` above
        // never finds it, so it would otherwise fall through to the generic "unsupported remote
        // pid" `ESRCH` below silently. Distinguish that specific case in the log so it reads as
        // "known, scoped-out signal delivery" rather than "the shim has no idea what this pid
        // is" -- the child is real and reachable via `sys_wait4`, just not signalable yet; see
        // `Process::cross_process_children`'s doc comment for the full scope statement.
        if let Some(pid) = pid
            && pid > 0
            && self.process().find_cross_process_child(pid).is_some()
        {
            log_unsupported!(
                "sys_kill with pid={pid}: signal delivery to a cross-process fork() child is not \
                 implemented (pass 141 documented gap -- wait4() works, kill() does not)"
            );
            return Err(Errno::ESRCH);
        }
        log_unsupported!("sys_kill with a remote pid that isn't a direct child");
        Err(Errno::ESRCH)
    }

    /// Returns whether there are any pending signals that can be delivered.
    pub(crate) fn has_pending_signals(&self) -> bool {
        let blocked = self.signals.blocked.get();
        let thread_pending = self.signals.pending.borrow().pending & !blocked;
        if !thread_pending.is_empty() {
            return true;
        }
        let shared_pending = self.signals.shared_pending.lock().pending & !blocked;
        !shared_pending.is_empty()
    }

    /// Returns the set of all pending (deliverable) signals.
    #[cfg(test)]
    pub(crate) fn pending_signal_set(&self) -> SigSet {
        let blocked = self.signals.blocked.get();
        let thread = self.signals.pending.borrow().pending & !blocked;
        let shared = self.signals.shared_pending.lock().pending & !blocked;
        thread | shared
    }

    /// Deliver any pending signals.
    pub(crate) fn process_signals(&self, ctx: &mut PtRegs) {
        loop {
            let blocked = self.signals.blocked.get();
            let (signal, siginfo) = {
                let mut pending = self.signals.pending.borrow_mut();
                if let Some(signal) = pending.next(blocked) {
                    (signal, pending.remove(signal))
                } else {
                    // Then try shared pending.
                    let mut shared = self.signals.shared_pending.lock();
                    if let Some(signal) = shared.next(blocked) {
                        (signal, shared.remove(signal))
                    } else {
                        break;
                    }
                }
            };
            if self.is_exiting() {
                // Don't deliver any more signals if exiting.
                return;
            }

            let action = self.signals.handlers.borrow().inner.lock()[signal].action;
            #[expect(clippy::match_same_arms)]
            match action.sigaction {
                SIG_DFL => {
                    match signal.default_disposition() {
                        SignalDisposition::Terminate
                        | SignalDisposition::Core
                        | SignalDisposition::Stop => {
                            // STOP is not currently supported, so treat as
                            // terminate. Core dumps are also not currently
                            // supported.
                            litebox_util_log::error!(
                                signal:? = signal,
                                pid:% = self.pid,
                                tid:% = self.tid;
                                "fatal signal: terminating task"
                            );
                            self.exit_group(ExitStatus::Signal(signal));
                        }
                        SignalDisposition::Ignore => {}
                        SignalDisposition::Continue => {
                            // Stop is not supported, so continue does nothing.
                        }
                    }
                }
                SIG_IGN => {}
                _ => {
                    #[cfg(target_arch = "aarch64")]
                    let sigreturn_trampoline = self.ensure_sigreturn_trampoline();
                    #[cfg(not(target_arch = "aarch64"))]
                    let sigreturn_trampoline = 0;
                    if let Err(DeliverFault) = self.signals.deliver_signal(
                        signal,
                        &siginfo,
                        &action,
                        ctx,
                        sigreturn_trampoline,
                    ) {
                        // Failed to deliver signal. Inject a SIGSEGV
                        // (terminating the process if we were trying to deliver
                        // a SIGSEGV).
                        self.force_signal(Signal::SIGSEGV, signal == Signal::SIGSEGV);
                    }
                }
            }
        }
    }

    /// Check whether the process-wide alarm deadline has passed and, if so,
    /// enqueue `SIGALRM`.
    ///
    /// Note this is a fallback in case the platform does not support timers.
    #[cfg(feature = "alarm_fallback")]
    #[inline]
    pub(crate) fn check_alarm_deadline(&self) {
        let mut alarm = self.process().alarm_timer.lock();
        if alarm.handle.is_some() {
            // If the platform supports timers, we rely on those to trigger SIGALRM, so we don't need
            // to check the deadline here.
            return;
        }
        if alarm
            .deadline
            .is_some_and(|deadline| self.global.platform.now() >= deadline)
        {
            alarm.deadline = None;
            self.send_shared_signal(
                litebox_common_linux::signal::Signal::SIGALRM,
                siginfo_kill(litebox_common_linux::signal::Signal::SIGALRM),
            );
        }
    }

    pub(crate) fn queue_signals(&self, signal: litebox_common_linux::signal::Signal) {
        if signal == litebox_common_linux::signal::Signal::SIGALRM {
            // The platform timer fired; clear the stored deadline so that a
            // subsequent `alarm()` call does not see a stale positive remaining
            // time due to timer imprecision (the timer can fire slightly before
            // the exact deadline).
            self.process().alarm_timer.lock().deadline = None;
        }
        self.send_shared_signal(signal, siginfo_kill(signal));
    }

    /// Returns whether the given signal is currently being ignored.
    fn is_signal_ignored(&self, signal: Signal) -> bool {
        // SIGKILL and SIGSTOP can never be ignored.
        if signal == Signal::SIGKILL || signal == Signal::SIGSTOP {
            return false;
        }
        // Blocked signals are never ignored, since the signal handler may
        // change by the time it is unblocked.
        if self.signals.blocked.get().contains(signal) {
            return false;
        }
        let handlers = self.signals.handlers.borrow();
        let inner = handlers.inner.lock();
        match inner[signal].action.sigaction {
            SIG_IGN => true,
            SIG_DFL => matches!(signal.default_disposition(), SignalDisposition::Ignore),
            _ => false,
        }
    }

    /// Only supports sending signals to self for now.
    pub(crate) fn send_signal(&self, signal: Signal, siginfo: Siginfo) {
        if self.is_signal_ignored(signal) {
            return;
        }
        self.signals
            .pending
            .borrow_mut()
            .push(&self.process().limits, signal, siginfo);
    }

    /// Sends a process-directed signal (stored in shared_pending).
    pub(crate) fn send_shared_signal(&self, signal: Signal, siginfo: Siginfo) {
        if self.is_signal_ignored(signal) {
            return;
        }
        self.signals
            .shared_pending
            .lock()
            .push(&self.process().limits, signal, siginfo);
    }

    /// Forces a signal to be delivered on next call to `check_for_signals`.
    fn force_signal(&self, signal: Signal, force_exit: bool) {
        let siginfo = Siginfo {
            signo: signal.as_i32(),
            errno: 0,
            code: SI_KERNEL,
            __pad: 0,
            data: SiginfoData::new_zeroed(),
        };
        self.force_signal_with_info(signal, force_exit, siginfo);
    }

    fn force_signal_with_info(&self, signal: Signal, force_exit: bool, siginfo: Siginfo) {
        // `handle_exception_request` maps every architectural trap (not just page faults) through
        // this path: SIGFPE (`#DE`), SIGTRAP (`#BP`), SIGILL (`#UD` -- notably reachable via
        // Windows' `STATUS_PRIVILEGED_INSTRUCTION`/`hlt` mapping in
        // `litebox_platform_windows_userland`, which is how musl mallocng's `a_crash()` abort
        // primitive is delivered to the guest), alongside the original SIGKILL/SIGSEGV callers.
        assert!(matches!(
            signal,
            Signal::SIGKILL | Signal::SIGSEGV | Signal::SIGFPE | Signal::SIGTRAP | Signal::SIGILL
        ));

        self.signals
            .pending
            .borrow_mut()
            .push(&self.process().limits, signal, siginfo);

        // Update the handler if necessary to ensure the signal is handled.
        let handlers = self.signals.handlers.borrow();
        let mut inner = handlers.inner.lock();
        let handler = &mut inner[signal];
        if force_exit
            || self.signals.blocked.get().contains(signal)
            || handler.action.sigaction == SIG_IGN
        {
            let mut blocked = self.signals.blocked.get();
            blocked.remove(signal);
            self.signals.set_signal_mask(blocked);
            handler.action = SigAction {
                sigaction: SIG_DFL,
                restorer: 0,
                flags: SaFlags::empty(),
                mask: SigSet::empty(),
                __pad: 0,
            };
            // Don't allow further changes to this action.
            handler.immutable = true;
        }
    }

    pub(crate) fn handle_exception_request(&self, info: &litebox::shim::ExceptionInfo) {
        #[cfg(target_arch = "x86_64")]
        let (signal, fault_address) = {
            let signal = match info.exception {
                Exception::DIVIDE_ERROR => Signal::SIGFPE,
                Exception::BREAKPOINT => Signal::SIGTRAP,
                Exception::INVALID_OPCODE => Signal::SIGILL,
                // Page faults and unknown exceptions map to SIGSEGV. There may be
                // more appropriate signals in some other cases (e.g., SIGBUS).
                _ => Signal::SIGSEGV,
            };
            // For page faults, provide the faulting address.
            let fault_address = if info.exception == Exception::PAGE_FAULT {
                info.cr2
            } else {
                0
            };
            (signal, fault_address)
        };
        // aarch64 has no hardware divide-trap: integer division by zero yields 0 (unsigned) or
        // an implementation-defined result (signed), never an exception, so there is no
        // DIVIDE_ERROR-equivalent mapping to make. Data/instruction aborts (the aarch64
        // page-fault-equivalent) carry the faulting address in FAR_EL1 (`info.fault_address`)
        // rather than a separate CR2-style register.
        #[cfg(target_arch = "aarch64")]
        let (signal, fault_address) = {
            let signal = match info.exception {
                Exception::BRK64
                | Exception::BREAKPOINT_LOWER_EL
                | Exception::BREAKPOINT_CURRENT_EL => Signal::SIGTRAP,
                Exception::DATA_ABORT_LOWER_EL
                | Exception::DATA_ABORT_CURRENT_EL
                | Exception::INSTRUCTION_ABORT_LOWER_EL
                | Exception::INSTRUCTION_ABORT_CURRENT_EL => Signal::SIGSEGV,
                // Unknown/unmapped exception classes are forwarded as SIGILL rather than
                // silently dropped -- matching real Linux's default disposition for an
                // undecoded synchronous exception.
                _ => Signal::SIGILL,
            };
            let fault_address = match info.exception {
                Exception::DATA_ABORT_LOWER_EL
                | Exception::DATA_ABORT_CURRENT_EL
                | Exception::INSTRUCTION_ABORT_LOWER_EL
                | Exception::INSTRUCTION_ABORT_CURRENT_EL => info.fault_address,
                _ => 0,
            };
            (signal, fault_address)
        };
        self.signals.last_exception.set(*info);
        self.force_signal_with_info(signal, false, siginfo_exception(signal, fault_address));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syscalls::tests::TestPlatform;

    #[test]
    fn clone_for_new_task_shares_signal_state_for_a_thread_but_not_a_forked_process() {
        // Regression test: `clone_for_new_task` used to share `shared_pending`/`handlers`
        // unconditionally, including for a genuine `fork()` -- meaning a process-directed signal
        // sent to either a forked child or its parent could land in (and be consumed by) the
        // other's queue instead, and a later `sigaction()` in either process would silently
        // change the other's handler too.
        let parent =
            SignalState::<TestPlatform>::new_process(Arc::new(Mutex::new(PendingSignals::new())));

        let thread_clone = parent.clone_for_new_task(None);
        assert!(
            Arc::ptr_eq(&parent.shared_pending, &thread_clone.shared_pending),
            "a new thread of the same process must share the process-wide pending-signal queue"
        );
        assert!(
            Arc::ptr_eq(&*parent.handlers.borrow(), &*thread_clone.handlers.borrow()),
            "a new thread of the same process must share signal handler dispositions"
        );

        let forked_child =
            parent.clone_for_new_task(Some(Arc::new(Mutex::new(PendingSignals::new()))));
        assert!(
            !Arc::ptr_eq(&parent.shared_pending, &forked_child.shared_pending),
            "a forked child must get its own independent pending-signal queue"
        );
        assert!(
            !Arc::ptr_eq(&*parent.handlers.borrow(), &*forked_child.handlers.borrow()),
            "a forked child must get its own independent (snapshotted) handler dispositions"
        );
    }
}
