// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

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
    /// Set once [`PidNamespaceTable::allocate`] has placed this namespace's first row, and cleared
    /// again when the slot is reclaimed.
    ///
    /// `create()` returns an id before any row exists for it (the caller's `allocate` runs a moment
    /// later), so "no row names this namespace" is NOT yet proof the namespace is dead -- it is
    /// equally what a clone mid-flight between the two calls looks like. Reclamation therefore only
    /// ever considers a `sealed` slot: one whose `allocate` has already run, so an empty table
    /// genuinely means its init and every descendant has exited.
    sealed: AtomicBool,
}

impl NamespaceSlot {
    const fn new() -> Self {
        Self {
            in_use: AtomicBool::new(false),
            parent: AtomicU32::new(INITIAL_NS),
            next_pid: AtomicI32::new(1),
            sealed: AtomicBool::new(false),
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
    /// How many namespaces `init_exited` has destroyed -- reported (throttled, at warn) so a run
    /// proves the teardown is actually firing, not merely enabled.
    destroyed: core::sync::atomic::AtomicUsize,
}

impl PidNamespaceTable {
    pub(crate) const fn new() -> Self {
        Self {
            namespaces: [const { NamespaceSlot::new() }; MAX_NAMESPACES],
            members: [const { MemberSlot::new() }; MAX_MEMBERS],
            destroyed: core::sync::atomic::AtomicUsize::new(0),
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
    /// (its init process, and every descendant, has exited).
    ///
    /// `reclaim_dead` is the `LITEBOX_PIDNS_RECLAIM_OFF` A/B switch: with it false this behaves
    /// exactly as it did before reclamation existed, which is what makes the leak's own symptom
    /// (`clone(CLONE_NEWPID)` answering `EAGAIN` after `MAX_NAMESPACES` calls, forever) reproducible
    /// from the same binary as the fix.
    pub(crate) fn create(&self, parent: u32, reclaim_dead: bool) -> Option<u32> {
        // Two passes, cheapest-first: prefer a slot no row names at all, then fall back to any free
        // slot (an id a dead-but-not-yet-recycled row still mentions is safe to reuse -- see
        // `is_referenced` -- but a genuinely untouched one is better).
        if let Some(id) = self.claim(parent, true) {
            return Some(id);
        }
        if let Some(id) = self.claim(parent, false) {
            return Some(id);
        }
        // THE LEAK, and the reason a long-lived guest used to run out of namespaces permanently:
        // `in_use` was set here and cleared NOWHERE in the tree, so every `clone(CLONE_NEWPID)` --
        // Chromium's zygote forks EVERY renderer/utility process that way -- retired one of the 256
        // slots for the whole session even though the namespace died with its init seconds later.
        // Once they were all gone, `create` answered `None`, `do_clone` turned that into `EAGAIN`,
        // `fork()` returned -1, and Chromium logged `Zygote could not fork: ... child_pid -1`.
        if reclaim_dead {
            let freed = self.reclaim_dead();
            if freed > 0 {
                litebox_util_log::debug!(
                    freed:% = freed;
                    "pidns: reclaimed dead namespace slots"
                );
                if let Some(id) = self.claim(parent, true) {
                    return Some(id);
                }
                if let Some(id) = self.claim(parent, false) {
                    return Some(id);
                }
            }
        }
        let (in_use, sealed, members) = self.occupancy();
        let stale = self.stale_rows();
        litebox_util_log::warn!(
            in_use:% = in_use, sealed:% = sealed, members:% = members, reclaim_dead:% = reclaim_dead,
            max_namespaces:% = MAX_NAMESPACES, max_members:% = MAX_MEMBERS,
            referenced_namespaces:% = stale.0, min_internal_pid:% = stale.1, max_internal_pid:% = stale.2,
            rows_per_namespace:? = self.rows_per_namespace(),
            sample_rows:? = &stale.3[..];
            "pidns: no free namespace slot -- clone(CLONE_NEWPID) refused with EAGAIN"
        );
        None
    }

    /// How many live rows each referenced namespace holds, as `[namespaces with exactly 1 row,
    /// ... exactly 2, ... exactly 3, ... 4 or more]` -- the discriminator between "the table is
    /// full of genuinely live, deeply-populated namespaces" (a capacity problem) and "full of
    /// one-row namespaces whose only member should have been released" (a leak).
    fn rows_per_namespace(&self) -> [usize; 4] {
        let mut counts = [0usize; MAX_NAMESPACES];
        for row in self.members.iter() {
            if row.ns_pid.load(Ordering::Acquire) == FREE_PID {
                continue;
            }
            let ns = row.ns.load(Ordering::Acquire);
            if let Some(slot) = usize::checked_sub(ns as usize, 1) {
                if let Some(c) = counts.get_mut(slot) {
                    *c += 1;
                }
            }
        }
        let mut buckets = [0usize; 4];
        for c in counts {
            match c {
                0 => {}
                1 => buckets[0] += 1,
                2 => buckets[1] += 1,
                3 => buckets[2] += 1,
                _ => buckets[3] += 1,
            }
        }
        buckets
    }

    /// `(namespaces still referenced, smallest live internal pid, largest live internal pid, up to
    /// 24 sample rows)` -- read at exhaustion to say whether the table is full of LIVE processes or
    /// of rows that should have been released when their process was reaped. Each sample row is
    /// `(namespace, pid in that namespace, internal pid)`.
    fn stale_rows(&self) -> (usize, i32, i32, alloc::vec::Vec<(u32, i32, i32)>) {
        let mut sample = alloc::vec::Vec::new();
        let mut min_ipid = i32::MAX;
        let mut max_ipid = i32::MIN;
        let mut referenced = [false; MAX_NAMESPACES];
        for row in self.members.iter() {
            let ns_pid = row.ns_pid.load(Ordering::Acquire);
            if ns_pid == FREE_PID {
                continue;
            }
            let ns = row.ns.load(Ordering::Acquire);
            let ipid = row.internal_pid.load(Ordering::Acquire);
            if let Some(slot) = usize::checked_sub(ns as usize, 1) {
                if let Some(flag) = referenced.get_mut(slot) {
                    *flag = true;
                }
            }
            if sample.len() < 24 {
                sample.push((ns, ns_pid, ipid));
            }
            min_ipid = min_ipid.min(ipid);
            max_ipid = max_ipid.max(ipid);
        }
        if sample.is_empty() {
            min_ipid = 0;
            max_ipid = 0;
        }
        (
            referenced.iter().filter(|f| **f).count(),
            min_ipid,
            max_ipid,
            sample,
        )
    }

    /// Claims the first free slot for a namespace nested inside `parent`.
    fn claim(&self, parent: u32, skip_referenced: bool) -> Option<u32> {
        for (index, slot) in self.namespaces.iter().enumerate() {
            if slot.in_use.load(Ordering::Acquire) {
                continue;
            }
            let id = index as u32 + 1;
            if skip_referenced && self.is_referenced(id) {
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
            slot.sealed.store(false, Ordering::Release);
            return Some(id);
        }
        None
    }

    /// Frees every slot that is `sealed` and that no member row names any more, returning how many
    /// were freed.
    ///
    /// A `sealed` slot with zero rows is dead by construction: the only way a row for a namespace
    /// disappears is [`Self::release`] forgetting a pid that has exited, and the namespace's own
    /// init (pid 1 there) is registered by the very `allocate` that sealed it. An UNsealed slot is
    /// never touched -- that is a `clone()` between `create` and `allocate`, legitimately rowless,
    /// and recycling it would hand its id to a second, concurrent clone.
    fn reclaim_dead(&self) -> usize {
        let mut freed = 0;
        for (index, slot) in self.namespaces.iter().enumerate() {
            if !slot.in_use.load(Ordering::Acquire) || !slot.sealed.load(Ordering::Acquire) {
                continue;
            }
            if self.is_referenced(index as u32 + 1) {
                continue;
            }
            // Reset every field, `in_use` last: until it is stored the slot still reads as taken,
            // so no concurrent `claim` can observe a half-reset slot.
            slot.parent.store(INITIAL_NS, Ordering::Release);
            slot.next_pid.store(1, Ordering::Release);
            slot.sealed.store(false, Ordering::Release);
            slot.in_use.store(false, Ordering::Release);
            freed += 1;
        }
        freed
    }

    /// `(slots in use, slots sealed, live member rows)` -- the whole table's occupancy, logged when
    /// a create or an allocate fails so the failure names which of the two limits it hit.
    fn occupancy(&self) -> (usize, usize, usize) {
        let mut in_use = 0;
        let mut sealed = 0;
        for slot in self.namespaces.iter() {
            if slot.in_use.load(Ordering::Acquire) {
                in_use += 1;
            }
            if slot.sealed.load(Ordering::Acquire) {
                sealed += 1;
            }
        }
        let members = self
            .members
            .iter()
            .filter(|row| row.ns_pid.load(Ordering::Acquire) != FREE_PID)
            .count();
        (in_use, sealed, members)
    }

    /// The namespace `ns`'s init (its pid 1) has EXITED, so Linux destroys the namespace: it kills
    /// everything else still in it and puts the namespace, reaped or not.
    ///
    /// Every row naming `ns` is dropped here -- that is the "everything else is killed" half, and
    /// it is what lets the slot actually become reusable. Rows in ANCESTOR namespaces are left
    /// alone: the init is still a zombie there until its parent reaps it, and the parent must still
    /// be able to `wait4`/read `si_pid` by the pid it knew. `release` (called from `sys_wait4`)
    /// clears those later, and `reclaim_dead` then frees the slot if that was the last one.
    pub(crate) fn init_exited(&self, ns: u32) {
        for row in self.members.iter() {
            if row.ns.load(Ordering::Acquire) == ns
                && row.ns_pid.load(Ordering::Acquire) != FREE_PID
            {
                row.internal_pid.store(FREE_PID, Ordering::Release);
                row.ns.store(INITIAL_NS, Ordering::Release);
                row.ns_pid.store(FREE_PID, Ordering::Release);
            }
        }
        // Nothing left in it: free the slot now rather than waiting for the next `create` to fail,
        // so a guest that forks in a tight loop never sees the table fill up at all.
        if !self.is_referenced(ns) {
            let Some(slot) = self.namespace(ns) else {
                return;
            };
            slot.parent.store(INITIAL_NS, Ordering::Release);
            slot.next_pid.store(1, Ordering::Release);
            slot.sealed.store(false, Ordering::Release);
            slot.in_use.store(false, Ordering::Release);
            let n = self
                .destroyed
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed)
                + 1;
            if n <= 2 || n % 64 == 0 {
                litebox_util_log::warn!(ns:% = ns, destroyed:% = n; "pidns: namespace destroyed with its init");
            }
        }
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
                        let (in_use, sealed, members) = self.occupancy();
                        litebox_util_log::warn!(
                            ns:% = current, in_use:% = in_use, sealed:% = sealed, members:% = members;
                            "pidns: namespace pid counter exhausted"
                        );
                        return None;
                    }
                    if !self.register(pid, current, internal_pid) {
                        let (in_use, sealed, members) = self.occupancy();
                        litebox_util_log::warn!(
                            ns:% = current, in_use:% = in_use, sealed:% = sealed, members:% = members,
                            max_members:% = MAX_MEMBERS;
                            "pidns: member table full -- clone refused with EAGAIN"
                        );
                        return None;
                    }
                    // Sealed only after the row is in place: from here an empty table means the
                    // namespace is dead, which is exactly the condition `reclaim_dead` tests.
                    slot.sealed.store(true, Ordering::Release);
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
