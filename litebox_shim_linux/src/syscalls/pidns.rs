//! PID namespaces (`CLONE_NEWPID`).
//!
//! A task sees the pids allocated in the namespace it belongs to: the child of a
//! `clone(CLONE_NEWPID)` is pid 1 of the brand-new namespace that clone created, and holds a pid in
//! every ancestor namespace too -- the parent's `wait4`/`kill`, `/proc/<pid>` and
//! `SCM_CREDENTIALS` all name it by the pid it has in THEIR namespace, which is why every level of
//! the chain is registered here rather than only the innermost.
//!
//! The initial namespace is the identity mapping: its pids are the internal ones, so nothing is
//! ever registered for it and every lookup short-circuits. Every other level's pid is a fresh
//! number from that namespace's own counter, never reused, which is what makes a single flat table
//! of `(namespace-local pid, namespace) -> internal pid` rows sufficient for all four directions:
//!
//! * `getpid`/`gettid`/`fork`'s return value -- the pid this task has in its own namespace (or its
//!   parent's, for `fork`'s), read straight out of the task.
//! * `kill`/`tgkill`/`wait4`'s pid argument -- [`Self::translate`], the caller's namespace in.
//! * `wait4`'s result, `siginfo`'s `si_pid`, `SCM_CREDENTIALS`' `pid` -- [`Self::pid_in`], the
//!   reader's namespace in.
//!
//! The table is a pointer-free field of `GlobalState`, so it lives in the cross-process shared
//! kernel arena and every host process in a `LITEBOX_PROCESS_FORK=1` family sees the same rows --
//! a parent in one host process can wait for, signal, or name a child that runs in another.
//! Internal pids stay the shared key everywhere else (`SharedProcessTable`, futexes, the pty
//! registry): a namespace never changes which process an internal pid means, only how it is
//! spelled.

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

/// The namespace every task starts in: the identity mapping, never registered, never freed.
pub(crate) const INITIAL_NS: u32 = 0;

const MAX_NAMESPACES: usize = 256;
const MAX_MEMBERS: usize = 2048;
const FREE_PID: i32 = 0;

struct NamespaceSlot {
    in_use: AtomicBool,
    parent: AtomicU32,
    next_pid: AtomicI32,
}

impl NamespaceSlot {
    const fn new() -> Self {
        Self {
            in_use: AtomicBool::new(false),
            parent: AtomicU32::new(INITIAL_NS),
            next_pid: AtomicI32::new(1),
        }
    }
}

/// One `(namespace-local pid, namespace, internal pid)` triple. `ns_pid == FREE_PID` marks a free
/// row; pids are allocated from 1 up, so a live row never holds 0.
struct MemberSlot {
    ns_pid: AtomicI32,
    ns: AtomicU32,
    internal_pid: AtomicI32,
}

impl MemberSlot {
    const fn new() -> Self {
        Self {
            ns_pid: AtomicI32::new(FREE_PID),
            ns: AtomicU32::new(INITIAL_NS),
            internal_pid: AtomicI32::new(FREE_PID),
        }
    }
}

pub(crate) struct PidNamespaceTable {
    namespaces: [NamespaceSlot; MAX_NAMESPACES],
    members: [MemberSlot; MAX_MEMBERS],
}

impl PidNamespaceTable {
    pub(crate) const fn new() -> Self {
        Self {
            namespaces: [const { NamespaceSlot::new() }; MAX_NAMESPACES],
            members: [const { MemberSlot::new() }; MAX_MEMBERS],
        }
    }

    /// `None` for the initial namespace, which has no slot of its own: it never allocates, never
    /// registers a row, and is every chain's last link (see [`Self::allocate`]).
    fn namespace(&self, ns: u32) -> Option<&NamespaceSlot> {
        if ns == INITIAL_NS {
            return None;
        }
        self.namespaces.get(ns as usize - 1)
    }

    /// Creates a namespace nested inside `parent`, returning its id, or `None` when every slot is
    /// taken by a namespace still in use.
    ///
    /// Slots are reclaimed, never refcounted: a namespace is dead once no live row mentions it
    /// (its init process, and every descendant, has exited), and a dead slot's id is never handed
    /// out twice while it is still referenced by a row.
    pub(crate) fn create(&self, parent: u32) -> Option<u32> {
        for attempt in 0..2 {
            for (index, slot) in self.namespaces.iter().enumerate() {
                if slot.in_use.load(Ordering::Acquire) {
                    continue;
                }
                let id = index as u32 + 1;
                if attempt == 0 && self.is_referenced(id) {
                    continue;
                }
                if slot
                    .in_use
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    continue;
                }
                slot.parent.store(parent, Ordering::Release);
                slot.next_pid.store(1, Ordering::Release);
                return Some(id);
            }
        }
        None
    }

    /// The namespace `ns` was created inside -- the namespace its own creator lives in.
    ///
    /// That is what a namespace's init (its pid 1) is named from the outside, so it is where a
    /// dying init's `SIGCHLD` `si_pid` has to be spelled: every other task in `ns` was created by
    /// another task of `ns`, so its parent is in `ns` itself.
    pub(crate) fn parent_of(&self, ns: u32) -> u32 {
        match self.namespace(ns) {
            Some(slot) => slot.parent.load(Ordering::Acquire),
            None => INITIAL_NS,
        }
    }

    fn is_referenced(&self, ns: u32) -> bool {
        self.members.iter().any(|row| {
            row.ns_pid.load(Ordering::Acquire) != FREE_PID
                && row.ns.load(Ordering::Acquire) == ns
        })
    }

    /// Registers `internal_pid` in every namespace from `ns` up to the initial one, returning the
    /// pid it was given in `ns` itself -- `1` for a namespace created by this very clone, a fresh
    /// number from the counter for one the child merely joins.
    ///
    /// `None` (with whatever rows were registered before it left in place, all for a pid that will
    /// never exist) only when the table is full; the caller turns that into `EAGAIN`.
    pub(crate) fn allocate(&self, ns: u32, internal_pid: i32) -> Option<i32> {
        if internal_pid <= 0 {
            return None;
        }
        let mut current = ns;
        let mut own = None;
        loop {
            let allocated = match self.namespace(current) {
                None => internal_pid,
                Some(slot) => {
                    let pid = slot.next_pid.fetch_add(1, Ordering::AcqRel);
                    if pid <= 0 {
                        return None;
                    }
                    if !self.register(pid, current, internal_pid) {
                        return None;
                    }
                    pid
                }
            };
            if own.is_none() {
                own = Some(allocated);
            }
            let parent = match self.namespace(current) {
                Some(slot) => slot.parent.load(Ordering::Acquire),
                None => break,
            };
            current = parent;
        }
        own.or(Some(internal_pid))
    }

    fn register(&self, ns_pid: i32, ns: u32, internal_pid: i32) -> bool {
        for row in self.members.iter() {
            if row
                .ns_pid
                .compare_exchange(FREE_PID, ns_pid, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            row.ns.store(ns, Ordering::Release);
            row.internal_pid.store(internal_pid, Ordering::Release);
            return true;
        }
        false
    }

    /// Forgets every row naming `internal_pid` -- a process's own pid when it exits, a thread's own
    /// tid when that thread does.
    pub(crate) fn release(&self, internal_pid: i32) {
        for row in self.members.iter() {
            if row.internal_pid.load(Ordering::Acquire) == internal_pid
                && row.ns_pid.load(Ordering::Acquire) != FREE_PID
            {
                row.internal_pid.store(FREE_PID, Ordering::Release);
                row.ns.store(INITIAL_NS, Ordering::Release);
                row.ns_pid.store(FREE_PID, Ordering::Release);
            }
        }
    }

    /// How `internal_pid` is spelled in `ns`: its own pid for the initial namespace (the identity
    /// mapping), its registered pid for one it belongs to, and `None` for a namespace that cannot
    /// see it at all -- which is what makes an out-of-namespace `kill` fail with `ESRCH`.
    pub(crate) fn pid_in(&self, internal_pid: i32, ns: u32) -> Option<i32> {
        if ns == INITIAL_NS {
            return Some(internal_pid);
        }
        self.members.iter().find_map(|row| {
            (row.ns_pid.load(Ordering::Acquire) != FREE_PID
                && row.ns.load(Ordering::Acquire) == ns
                && row.internal_pid.load(Ordering::Acquire) == internal_pid)
                .then(|| row.ns_pid.load(Ordering::Acquire))
        })
    }

    /// Which internal pid a caller in `ns` means by `ns_pid`, or `None` when `ns` has no such pid.
    pub(crate) fn translate(&self, ns_pid: i32, ns: u32) -> Option<i32> {
        if ns_pid <= 0 {
            return None;
        }
        if ns == INITIAL_NS {
            return Some(ns_pid);
        }
        self.members.iter().find_map(|row| {
            (row.ns_pid.load(Ordering::Acquire) == ns_pid && row.ns.load(Ordering::Acquire) == ns)
                .then(|| row.internal_pid.load(Ordering::Acquire))
        })
    }
}
