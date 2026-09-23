//! Lazy (reserve-then-commit-on-first-fault) memory population for cross-process
//! `LITEBOX_PROCESS_FORK=1` fork children -- the mechanism 76th-82nd pass's own RAM-crater
//! investigation converged on as the only remaining safe lever (see `AGENTS.md`'s "Where things
//! stand" section as of the pass that added this file).
//!
//! # KNOWN UNSAFE CASE -- do not flip `LITEBOX_LAZY_FORK_COMMIT=1` on for a real boot yet
//!
//! **Verified safe and a real, measured win** for a fork that `execve()`s essentially immediately
//! (the dominant real-world case, ~85% of fork-exec cycles per the 79th pass's own measurement):
//! 5/5 clean runs, both debug and release, `bash -c 'echo hello; sleep 0.2; echo done'`
//! (the `sleep` fork execve's almost instantly) -- correct output, exit 0, real ~3-18x
//! parent-side group-copy speedup measured both builds (debug: 103ms eager vs 34ms mixed / 6
//! groups, 2 lazy; release: 6ms mixed vs a proportionally larger eager baseline). Re-verified
//! clean 5/5, both builds, 85th pass, after the fix below landed.
//!
//! **NOT yet safe for a fork that keeps running WITHOUT `execve()`ing** (e.g. a bash `(...)`
//! subshell, which runs bash's own already-forked, already-copied code directly rather than
//! replacing its address space): `bash -c '(echo subshell_child; x=inner_var; echo $x); echo
//! parent_after'` originally crashed 5/5 (83rd/84th passes). The 85th pass found this was
//! actually TWO SEPARATE bugs layered on top of each other:
//!
//! **Bug A (FIXED, 85th pass)** -- the crash's own address (~0xff000 bytes above the lazy stack
//! group's own end, `PAGE_READONLY`, matching the synthesized x86_64 sigreturn trampoline's own
//! deliberately-execute-disabled page -- see `litebox_shim_linux::LinuxShimEntrypoints::exception`'s
//! doc comment) was a red herring: that page's own memory was always correct (real, eagerly
//! committed, correct protection) both with and without the lazy flag, and the fault there IS the
//! trampoline mechanism working exactly as designed. The real defect was that
//! [`classify_lazy_eligible_groups`] could make the group containing the child's OWN LIVE `%rsp`
//! AT FORK TIME lazy -- confirmed live via `LITEBOX_DIAG_LAZY_FORK_COMMIT=1`: the crash fired with
//! **zero** `[lazy_fork_commit] fault #N serviced` lines ever printed, i.e. `lazy_commit_veh` never
//! got to service a single real page fault before the process died to an unhandled
//! `STATUS_SINGLE_STEP`/wild-jump cascade. Every exception Windows delivers -- not just ones whose
//! own fault address lands in a lazy range -- is delivered with `CONTEXT.Rsp` set to whatever the
//! CPU held at fault time; if that happens to be an address inside a group this module reserved
//! but has NEVER YET serviced (guaranteed true for the group anchoring the child's very first
//! instructions), downstream fault-handling code that assumes a live thread's own current stack is
//! at minimum readable breaks in ways this repro's specific crash signature never disambiguated
//! cleanly from a dozen other candidate theories the 83rd/84th passes tried and refuted. Fix:
//! [`classify_lazy_eligible_groups`] now takes the child's fork-time `%rsp` and excludes whichever
//! group contains it, leaving every OTHER pure-data group (heap, etc.) lazy exactly as before --
//! confirmed live: the exact subshell repro's original crash signature (0 faults serviced,
//! `Killed`, `inner_var` never printed) is gone, 5/5, both debug and release.
//!
//! **Bug B (OPEN, found by the 85th pass while verifying Bug A's fix)** -- with Bug A fixed, the
//! SAME subshell repro now runs further (lazy faults ARE serviced, 3/3 with real parent data, not
//! zero-filled) but still fails 5/5, both builds, with a NEW, different, equally deterministic
//! signature: `Fatal glibc error: malloc.c:2601 (sysmalloc): assertion failed: ...` -- SIGABRT, not
//! SIGKILL, and `subshell_child` now prints but `inner_var` still never does. This is a genuine
//! heap-corruption assertion, and the mechanism is structural, not incidental: [`lazy_commit_veh`]
//! services a fault by `ReadProcessMemory`-ing the PARENT's address space **at whatever moment the
//! CHILD happens to touch that page**, which for a fork-WITHOUT-`execve` child can be arbitrarily
//! long after `fork()` returned control to the PARENT's own guest thread. The eager `copy_one_group`
//! path captures every byte synchronously, before the parent's guest thread ever resumes, so it is
//! immune to this; this module's whole point is deferring that capture, which only stays correct if
//! the parent's memory at the same virtual address is guaranteed stable in the meantime -- true for
//! a page the parent never touches again, false for one it does (e.g. its own heap, mutated by its
//! own continued malloc/free activity while the child is still running). The observed corruption
//! (a `malloc_state.top`-chunk consistency check) is exactly the shape a torn/inconsistent read of a
//! live, concurrently-mutating heap would produce. **This is a genuine TOCTOU (time-of-check to
//! time-of-use) gap in the module's own "Correctness argument" section below, which only proves the
//! CHILD's own access-based deferral is safe and never establishes that the PARENT's memory at the
//! same address stays stable after `fork()` returns -- a separate assumption, true for the eager
//! path by construction, false in general for the lazy path.** No fix attempted this pass; a real
//! fix needs either a synchronous fork-time SNAPSHOT of lazy-eligible bytes into a buffer the lazy
//! fault handler reads from instead of the live parent (trades away part of the eager-copy cost this
//! module exists to avoid, but only for eligible groups, and only the bytes that are ever actually
//! touched could still be deferred if the snapshot itself is lazy-populated on the PARENT side at
//! reservation time... an unexplored design), or genuine OS-level COW between the two separate
//! Windows processes (no such primitive is being used today; a real design pass, likely on the scale
//! of `ADVISORY-002`, not a quick fix).
//!
//! **Because of Bug B, [`lazy_fork_commit_enabled`] stays an explicit opt-in
//! (`LITEBOX_LAZY_FORK_COMMIT=1`), default OFF, and this module's own existence changes NOTHING
//! about the default fork path** (confirmed live, 85th pass: 5/5 clean runs of both repros above
//! with the env var unset, byte-identical correct output to before this module existed). Do not
//! flip this on for `.wfgy/webtop_stack.sh` or any real boot attempt until Bug B is fixed -- a real
//! desktop boot forks many long-lived processes (shells, daemons, e.g. `dbus-daemon`) that do not
//! immediately `execve()` AND keep running concurrently with a parent that keeps mutating its own
//! heap, which is exactly Bug B's trigger shape -- worse and harder to isolate than this clean,
//! minimal, 100%-reproducible standalone repro.
//!
//! # 86th pass -- both proposed real fixes for Bug B investigated in depth, NEITHER implemented;
//! a THIRD candidate found and scoped, also not implemented this pass. Flag stays default OFF.
//!
//! Re-verified live (debug build, unmodified binary) before starting: flag unset, subshell repro
//! 100% clean (`subshell_child`/`inner_var`/`parent_after` all print); flag set, subshell repro
//! still hits the exact documented Bug B signature (`Fatal glibc error: malloc.c:2601 (sysmalloc):
//! assertion failed`). Both match this file's own prior claims byte-for-byte -- nothing had
//! drifted since the 85th pass.
//!
//! **Candidate 2 (genuine OS-level section-object COW) -- investigated via source reading,
//! concluded structurally infeasible without a disruptive full-allocator rewrite, NOT attempted.**
//! Every guest memory allocation in this crate -- [`WindowsUserland`]'s `allocate_pages`/
//! `deallocate_pages`/`update_permissions`/`unmap_shared_memory` (the entire `PageManagementProvider`
//! impl backing guest `mmap`/`munmap`/`mprotect`/heap/stack) -- is built exclusively on PRIVATE
//! `VirtualAlloc2(MEM_RESERVE|MEM_COMMIT)` regions, never a section object; retrofitting real
//! `MapViewOfFile3`/`PAGE_WRITECOPY` COW at fork time requires the memory to have been
//! SECTION-BACKED from the point of allocation, not converted after the fact (Windows has no
//! "adopt this already-populated private VirtualAlloc range into a section" primitive -- the only
//! way to move existing bytes into a section is to copy them, which is the exact per-byte cost
//! this whole mechanism exists to avoid, and would have to be paid once per allocation rather than
//! once per fork if done "from the start"). Two concrete, code-level reasons this is a disruptive
//! rewrite, not a scoped fix:
//!   1. `deallocate_pages` (`lib.rs`, ~line 8469-8496) EXPLICITLY REFUSES to decommit a `MEM_MAPPED`
//!      section view, logging an error and leaving it alone, because `VirtualFree(MEM_DECOMMIT)` is
//!      only valid on privately-committed memory. A real Linux `munmap()` can unmap an arbitrary
//!      sub-range of a larger mapping; a Windows section view can only be unmapped WHOLE (partial
//!      "unmap" needs the `MEM_RESERVE_PLACEHOLDER`/`MEM_REPLACE_PLACEHOLDER` split/coalesce dance).
//!      Every guest `munmap`/`mprotect` sub-range call on a section-backed group would need this
//!      placeholder machinery, not a `VirtualFree` one-liner.
//!   2. This exact codebase already fought this exact battle, for ONE fixed-size, fixed-address,
//!      narrowly-scoped region (`SHARED_KERNEL_HEAP_BASE`/`SHARED_KERNEL_HEAP_SIZE`, `lib.rs`
//!      ~line 10355-10394's own doc comment) and the trail is a live record of how fragile it is:
//!      plain `SEC_RESERVE` sections, `MEM_RESERVE`-type views, and the documented
//!      `MEM_RESERVE_PLACEHOLDER`/`MEM_REPLACE_PLACEHOLDER` pair ALL failed `ERROR_INVALID_ADDRESS`
//!      at that fixed address (only a genuinely `SEC_COMMIT` section succeeded there, which then had
//!      to be immediately `VirtualFree(MEM_DECOMMIT)`-ed to avoid charging the FULL size against
//!      system commit at creation time -- a section is not lazily committed by default, another trap
//!      this design would have to reopen); a fixed placement at a high canonical address is also not
//!      collision-guaranteed against ASLR (`SHARED_KERNEL_HEAP_ACTUAL_BASE`'s own fallback path,
//!      confirmed live via `cdb`, `STATUS_CONFLICTING_ADDRESSES`). Reproducing this fight for a
//!      SINGLE bounded region already needed several iterations; the guest VMA allocator handles an
//!      open-ended number of dynamically-sized, dynamically-placed, `MAP_FIXED`-capable regions
//!      across a real boot's whole process tree -- the same class of problem at much larger, less
//!      bounded scope. This is exactly the "too large/risky for one pass" case the task's own
//!      fallback anticipates -- not attempted.
//!
//! **Candidate 1 (fork-time snapshot buffer) -- investigated rigorously, concluded it cannot
//! preserve the dominant fork-then-execve case's own measured win, so it is not a net improvement,
//! NOT attempted.** Any snapshot that is actually race-free must be captured SYNCHRONOUSLY while
//! the parent's guest thread is still blocked inside the fork syscall handler -- exactly the window
//! [`reserve_group_lazy`] currently does nothing in, and exactly the window the EAGER
//! `copy_one_group` path already exploits for its own immunity to Bug B (see this file's own
//! "Correctness argument" section, and the 81st pass's `AGENTS.md` entry, which already proved
//! there is no way to defer POPULATION past this point without reopening a `fork()`-without-
//! `execve()` write race -- the same argument applies unchanged to deferring the READ/CAPTURE side
//! for a snapshot). Capturing a stable snapshot synchronously means touching (reading) every byte
//! of every eligible group at fork time, REGARDLESS of whether the child ever touches that memory
//! -- because at fork time nothing yet distinguishes a fork that is about to `execve()` (the
//! dominant, ~85% case, 79th pass) from one that is not. That read is the same order of cost as
//! the eager path's own `ReadProcessMemory` loop this mechanism exists to avoid paying for the
//! dominant case; storing the captured bytes somewhere other than the child's own committed guest
//! memory (e.g. a scratch file) avoids re-adding the CHILD's `VirtualAlloc(MEM_COMMIT)` charge for
//! untouched pages, but does nothing for the TIME cost, which is what the 83rd pass's own measured
//! win (103ms eager vs 34ms mixed, debug; proportionally larger release win) was actually about --
//! net effect, a "safe" snapshot design would make EVERY fork pay full eager-equivalent read cost
//! unconditionally, which is strictly worse than simply keeping such groups on the existing eager
//! `copy_one_group` path (same read cost, but writes directly into place with no extra buffer hop
//! or later re-fault indirection). Not a net improvement -- not attempted.
//!
//! **Candidate 3 (found this pass, not part of the original two): software COW via guard pages,
//! symmetric with this module's own existing child-side VEH.** Sketch: at fork time, instead of
//! doing nothing (today's [`reserve_group_lazy`]) for an eligible group's ALREADY-COMMITTED pages
//! in the PARENT, `VirtualProtect` them to `PAGE_READONLY` (an O(1) call per contiguous committed
//! sub-range, not O(bytes) -- cheap, same performance class as the rest of this mechanism). Install
//! a SECOND VEH, on the PARENT side, that catches the parent's own next WRITE fault to such a page:
//! on that fault, snapshot the page's CURRENT (pre-write) bytes into a small side buffer, restore
//! `PAGE_READWRITE` on just that one page so the write can retry and succeed, and record that a
//! snapshot now exists for that address. The CHILD's existing [`lazy_commit_veh`] would then, on
//! its own first fault, prefer a recorded snapshot over a live `ReadProcessMemory` if one exists
//! (closing Bug B for the single-fork-child case: the child's data is now genuinely stable, either
//! because the parent has not yet raced ahead of it, protected by `PAGE_READONLY`, or because a
//! stable pre-write snapshot was captured at the exact moment the parent tried to). **This is
//! correctness-sound for exactly one live fork child at a time, but a real desktop boot needs up
//! to `litebox_shim_linux::GlobalState`'s `live_cross_process_fork_children` admission control's
//! own cap of 6 CONCURRENT long-lived children forking from the same continuously-mutating parent
//! (`AGENTS.md`'s 76th-pass finding) -- and a
//! single global "protect once, snapshot once, unprotect" cycle is provably wrong for 2+ overlapping
//! generations: if child A forks, the parent later write-faults and captures+unprotects a page, and
//! child B THEN forks (after that write), B's own fault handler must NOT reuse A's stale pre-write
//! snapshot -- it needs the parent's CURRENT (post-write) value as of B's own later fork moment,
//! which requires either re-protecting on every new fork (itself fine, cheap) or a real
//! multi-generation/multi-version scheme so a later child's read cannot be served A's earlier
//! snapshot. Getting this fully correct is a genuine concurrent multi-version-COW design problem
//! (per-page generation tracking, fan-out to N potentially-still-behind children on a single parent
//! write, race-free coordination between the parent's own write-fault handler and every live
//! child's independent read-fault handler touching the same physical page) -- the same order of
//! design complexity as this module's OWN existing single-generation, single-direction mechanism,
//! which took three consecutive full passes (83rd-85th) of live `cdb`/diagnostic-gated iteration to
//! get right even in its simpler form. Attempting the full multi-generation version without an
//! equivalent live-debug budget in one pass risks shipping an unverified, silently-wrong-data
//! mechanism (not merely a crash) -- worse than the current honest crash, and exactly what the
//! standing goal forbids. **Not implemented this pass.** Most promising narrower first cut, left
//! for a future pass to scope further: force any group affected by a SECOND concurrent live
//! fork-child (i.e. only take the fast single-generation guard-page path when at most one
//! outstanding, not-yet-`execve()`'d/not-yet-exited child could still be depending on that parent's
//! memory; fall back to eager copy the moment a second overlapping fork would otherwise need to
//! share the same protected page) -- trades away laziness only under real multi-child contention,
//! which live 76th-pass evidence suggests is common during a real boot's fork storm but not
//! universal, while keeping the fast path for the common single-active-fork-child moment. Needs its
//! own live-verification pass before being trusted, exactly as this module's other two bugs did.
//!
//! **Conclusion: [`lazy_fork_commit_enabled`] stays default OFF, unchanged this pass. No runtime
//! behavior was modified.** `DE_UP` was not attempted (the flag remains unsafe to enable for a real
//! boot). See `AGENTS.md`'s 86th-pass entry for the compact version of this writeup.
//!
//! # Why this exists
//!
//! `copy_one_group` (this crate's `process_fork` module) reserves AND fully commits AND
//! byte-copies every fork-carried VMA group EAGERLY, at fork time, in the parent, before the
//! child ever runs a single instruction -- regardless of whether the child immediately
//! `execve()`s (measured: ~85% of fork-exec cycle time wasted on copying data an `execve` throws
//! away moments later) or whether the region is mostly untouched (a guest's 8 MiB stack region is
//! the canonical example: only a handful of pages near the top are ever really touched).
//!
//! This module implements the alternative: for ELIGIBLE groups (see
//! [`classify_lazy_eligible_groups`]'s doc comment for exactly which groups qualify), the parent
//! only `VirtualAlloc2`s the exact span with `MEM_RESERVE` (no `MEM_COMMIT`, no data copied at
//! all) at fork time, and the CHILD installs a Vectored Exception Handler that commits + populates
//! each page LAZILY, the first time the child's own guest code actually touches it (read OR
//! write -- population happens before the access is allowed to complete either way, so there is
//! no window where the child could observe uninitialized memory).
//!
//! # Correctness argument (why this is safe where a timing-based "defer the whole copy" trick is
//! not -- see the 81st pass's entry in `AGENTS.md` for the timing trick this deliberately is NOT)
//!
//! A plain `fork()`ed child has no POSIX guarantee it won't touch its own (logically
//! copy-on-write) memory before its first syscall, so any mechanism that defers population based
//! on TIME or on "has the child made a syscall yet" can race a child that writes immediately.
//! This mechanism defers based on ACCESS, not time: the very first load or store to a byte in a
//! lazily-reserved range takes a hardware page fault (the page genuinely is not present -- Windows
//! reports `EXCEPTION_ACCESS_VIOLATION` the same way it would for any other not-yet-committed
//! `MEM_RESERVE` region), the VEH handler catches that EXACT fault, synchronously populates the
//! page from the parent's real data at that address, and only then returns
//! `EXCEPTION_CONTINUE_EXECUTION`, which re-executes the very same faulting instruction -- now
//! against real, correct data. There is no way for the child to observe stale/zero/uninitialized
//! content, because the hardware itself refuses the access until this handler has run.
//!
//! # Scope of this first landing (deliberately narrow)
//!
//! Only PURE-DATA groups (no `VM_EXEC` byte anywhere in the group's covering `vma_layout`
//! ranges) are made lazy. CODE groups keep using the existing eager `copy_one_group` path
//! unconditionally. This sidesteps the separate, already-fragile EXEC-permission-fixup pass in
//! `process_fork.rs` (`spawn_process_fork_child`'s post-copy `vma_layout` walk, PASS 144) entirely
//! -- that pass only ever iterates VMAs with `VM_EXEC` set, so a group this module claims never
//! touches it. Every page this module commits is committed as blanket `PAGE_READWRITE`, matching
//! `copy_one_group`'s own existing behavior for every non-exec page today (that function has never
//! narrowed permissions for read-only DATA pages either -- see PASS 144's own doc comment: "A
//! region with none of READ/WRITE/EXEC set is left at the group's default `PAGE_READWRITE`"), so
//! this is not a new permissiveness regression relative to the eager path it replaces.
//!
//! Gated behind `LITEBOX_LAZY_FORK_COMMIT=1`, default OFF: with the flag unset,
//! [`classify_lazy_eligible_groups`] always returns an empty set, every group takes the existing
//! eager `copy_one_group` path exactly as before, and the child never installs this module's VEH
//! handler at all (see [`install_if_configured`]'s early return) -- so the entire default/existing
//! behavior of every process this project already relies on is provably untouched by this file's
//! mere existence.
//!
//! # Concurrency
//!
//! Deliberately lock-free: the VEH handler may run on multiple guest threads concurrently
//! faulting on the SAME page. `VirtualAlloc(MEM_COMMIT)` on an already-committed page and
//! `ReadProcessMemory`+copy of the same bytes twice are both idempotent and harmless -- so no
//! "already populated" bit, no mutex, no risk of a fault handler blocking on a lock another
//! faulting thread holds. The lazy-range table itself ([`LAZY_RANGES`]) and the parent handle
//! ([`PARENT_HANDLE`]) are written exactly ONCE, by [`install_if_configured`], before the child
//! resumes any guest execution, and never mutated again -- `OnceLock`'s own publication fence
//! makes that one write visible to every later reader on every thread without further
//! synchronization.

use core::ffi::c_void;
use core::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::ops::Range;
use std::sync::OnceLock;

use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
    EXCEPTION_POINTERS, ReadProcessMemory,
};
use windows_sys::Win32::System::Memory::{
    MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER, MEM_EXTENDED_PARAMETER_0,
    MEM_EXTENDED_PARAMETER_1, MEM_RESERVE, MemExtendedParameterAddressRequirements,
    PAGE_READWRITE, VirtualAlloc, VirtualAlloc2,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_VM_READ};

use crate::process_fork::GroupCopyResult;

pub type Handle = windows_sys::Win32::Foundation::HANDLE;

const PAGE_SIZE: usize = 4096;
const VM_EXEC: u32 = 1 << 2;

/// Opt-in flag: `LITEBOX_LAZY_FORK_COMMIT=1` enables lazy reserve-on-fault population for eligible
/// (pure-data) fork-carried groups. Unset (the default) preserves the pre-existing eager
/// `copy_one_group` behavior for EVERY group, unconditionally.
#[must_use]
pub fn lazy_fork_commit_enabled() -> bool {
    std::env::var_os("LITEBOX_LAZY_FORK_COMMIT").is_some()
}

/// Internal-only marker env var: carries the set of group spans the parent reserved (but did NOT
/// commit or copy) as `start-end` hex pairs, comma-separated, e.g. `"7f0000-810000,900000-1100000"`.
/// Consumed by [`install_if_configured`] in the child. Never guest-visible.
pub const FORK_CHILD_LAZY_RANGES_ENV_VAR: &str = "LITEBOX_INTERNAL_FORK_CHILD_LAZY_RANGES";

/// Internal-only marker env var: the parent's own PID, decimal, so the child can `OpenProcess`
/// with `PROCESS_VM_READ` and pull real data from the parent's address space on each lazy fault.
/// Only set (by the parent) when [`FORK_CHILD_LAZY_RANGES_ENV_VAR`] is non-empty. Never
/// guest-visible.
pub const FORK_CHILD_PARENT_PID_ENV_VAR: &str = "LITEBOX_INTERNAL_FORK_CHILD_PARENT_PID";

/// Parent-side: given the SAME `group_relocations`/`vma_layout` `spawn_process_fork_child`
/// already has in hand, returns the subset of `group_relocations` eligible for lazy treatment --
/// empty unless [`lazy_fork_commit_enabled`], and always excluding any group whose covering
/// `vma_layout` range(s) include `VM_EXEC` (see this module's own doc comment for why CODE stays
/// on the eager path). Order matches `group_relocations`' own order; callers use this to decide,
/// per group, whether to call [`reserve_group_lazy`] instead of `copy_one_group`.
///
/// Also excludes whichever group contains `active_rsp` -- the child's OWN guest `%rsp` at the
/// exact moment of this `fork()` call, i.e. the live stack the child's very first (and every
/// subsequent) exception will be DELIVERED with as `CONTEXT.Rsp`, regardless of what the fault is
/// actually about. Root-caused (85th pass) as Bug 3 (`AGENTS.md`'s 84th-pass entry): the subshell
/// (fork-without-`execve`) repro's guest `%rsp` at fork time lands inside the SAME lazily-reserved
/// (`MEM_RESERVE`-only, zero pages ever committed) 8 MiB+64 KiB stack group `classify_lazy_eligible_
/// groups` was already making lazy for exactly this repro -- confirmed live via
/// `LITEBOX_DIAG_LAZY_FORK_COMMIT=1`: the crash fires with ZERO `[lazy_fork_commit] fault #N
/// serviced` lines ever printed, i.e. the mechanism never even got to service its first real page
/// fault. The crash itself is an UNRELATED `EXCEPTION_ACCESS_VIOLATION` (an instruction fetch at
/// the synthesized sigreturn trampoline, a separate, always-eagerly-committed page one page past
/// this group's own end) -- so the fault address itself is never inside a lazy range, and
/// `lazy_commit_veh` correctly declines it (`EXCEPTION_CONTINUE_SEARCH`). The break is that
/// Windows delivers THIS (and every) exception using `CONTEXT.Rsp` exactly as the CPU held it at
/// fault time -- the guest's own live, GUEST-address `%rsp` -- and with `LITEBOX_LAZY_FORK_COMMIT
/// =1`, that value pointed into memory that had NEVER been committed by anyone: not by the eager
/// path (this group was reserved-only), and not yet by a lazy fault either (nothing had touched
/// this exact page since the fork). A page fully absent from the process's own working set at the
/// moment its address becomes `CONTEXT.Rsp` for exception delivery is a scenario the ordinary
/// "stack-touching instruction lazily faults, gets serviced, retries" path (proven correct,
/// 83rd/84th passes, for the dominant fork-then-`execve` case) never exercises, because there the
/// fault address and `CONTEXT.Rsp` are the SAME address, serviced by `lazy_commit_veh` before
/// anything else can look at it. Pre-committing (eager, `copy_one_group`) the one group the
/// child's live stack pointer sits in at fork time closes this gap while leaving every OTHER
/// pure-data group (heap, TLS, etc.) lazy exactly as before -- a narrow, targeted fix, not a
/// reversion of the whole feature: real measurement (85th pass, `LITEBOX_DIAG_FORK_TIMING=1`)
/// shows the excluded group is consistently one of the smallest eligible ones per real fork (this
/// exact repro: 2 groups total, ~8.06 MiB and ~4.19 KiB; the 8 MiB one is what gets excluded here,
/// but real forks were already observed classifying MULTIPLE separate stack-shaped groups, and
/// only the one actually anchoring `active_rsp` loses its lazy treatment).
#[must_use]
pub fn classify_lazy_eligible_groups(
    group_relocations: &[(Range<usize>, usize)],
    vma_layout: &[(Range<usize>, u32, bool)],
    active_rsp: usize,
) -> Vec<bool> {
    if !lazy_fork_commit_enabled() {
        return vec![false; group_relocations.len()];
    }
    group_relocations
        .iter()
        .map(|(group, _dest_base)| {
            // Exclude the group the instant ANY overlapping vma_layout range carries VM_EXEC --
            // conservative by design: a group is data-only, and therefore lazy-eligible, only if
            // every vma_layout range touching it agrees.
            let has_exec = vma_layout
                .iter()
                .any(|(range, flags, _)| ranges_overlap(range, group) && flags & VM_EXEC != 0);
            // Exclude the group anchoring the child's own live stack pointer at fork time -- see
            // this function's own doc comment for the full root-cause/correctness argument.
            let is_active_stack = group.contains(&active_rsp);
            !has_exec && !is_active_stack
        })
        .collect()
}

fn ranges_overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

/// Serializes the given group spans as the `FORK_CHILD_LAZY_RANGES_ENV_VAR` value.
#[must_use]
pub fn serialize_lazy_ranges(ranges: &[Range<usize>]) -> String {
    ranges
        .iter()
        .map(|r| format!("{:x}-{:x}", r.start, r.end))
        .collect::<Vec<_>>()
        .join(",")
}

fn deserialize_lazy_ranges(s: &str) -> Vec<Range<usize>> {
    s.split(',')
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let (a, b) = part.split_once('-')?;
            let start = usize::from_str_radix(a, 16).ok()?;
            let end = usize::from_str_radix(b, 16).ok()?;
            (start < end).then_some(start..end)
        })
        .collect()
}

/// Parent-side: reserves (but does NOT commit or copy) `source_group`'s exact span at its exact
/// SOURCE address in the child, mirroring `copy_one_group`'s own step 1 (same
/// `MEM_ADDRESS_REQUIREMENTS`-forced `VirtualAlloc2` call, same hard-fail-on-address-mismatch
/// discipline) but stopping there -- no `WriteProcessMemory`, no per-page loop. The actual
/// population happens later, lazily, in the CHILD, via this module's VEH handler
/// ([`lazy_commit_veh`]), once [`install_if_configured`] has told it which ranges to expect.
#[must_use]
pub fn reserve_group_lazy(child: Handle, source_group: &Range<usize>) -> GroupCopyResult {
    let len = source_group.len();
    let fail = |err: u32| GroupCopyResult {
        source_group: source_group.clone(),
        succeeded: false,
        last_error: err,
    };
    let mut addr_req = MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: source_group.start as *mut c_void,
        HighestEndingAddress: (source_group.end - 1) as *mut c_void,
        Alignment: 0,
    };
    let mut ext_param = MEM_EXTENDED_PARAMETER {
        Anonymous1: MEM_EXTENDED_PARAMETER_0 {
            _bitfield: MemExtendedParameterAddressRequirements as u64,
        },
        Anonymous2: MEM_EXTENDED_PARAMETER_1 {
            Pointer: (&raw mut addr_req).cast::<c_void>(),
        },
    };
    let reserved = unsafe {
        VirtualAlloc2(
            child,
            core::ptr::null_mut(),
            len,
            MEM_RESERVE, // NOTE: reserve only -- no MEM_COMMIT, unlike copy_one_group.
            PAGE_READWRITE,
            &raw mut ext_param,
            1,
        )
    };
    if reserved.is_null() {
        return fail(unsafe { windows_sys::Win32::Foundation::GetLastError() });
    }
    if reserved as usize != source_group.start {
        unsafe {
            windows_sys::Win32::System::Memory::VirtualFreeEx(
                child,
                reserved,
                0,
                windows_sys::Win32::System::Memory::MEM_RELEASE,
            );
        }
        return fail(0);
    }
    GroupCopyResult {
        source_group: source_group.clone(),
        succeeded: true,
        last_error: 0,
    }
}

// ---- Child-side state, populated exactly once by install_if_configured ----

static LAZY_RANGES: OnceLock<Vec<Range<usize>>> = OnceLock::new();
/// Cached `OpenProcess(PROCESS_VM_READ, ..., parent_pid)` handle, as a raw pointer value (0 =
/// unset). `HANDLE` (`isize`/`*mut c_void`-shaped) is not itself `Sync`, so it is stored as an
/// `AtomicIsize` and cast back to `Handle` at each use -- read-only after
/// [`install_if_configured`]'s single write.
static PARENT_HANDLE: AtomicIsize = AtomicIsize::new(0);
/// Diagnostic-only (`LITEBOX_DIAG_LAZY_FORK_COMMIT=1`): total lazy faults this process's own
/// [`lazy_commit_veh`] has serviced, and how many of those found the parent's data unreadable
/// (left zero-filled). Never consulted for correctness -- purely observational counters for
/// verifying the mechanism actually engages during a real fork (as opposed to every group being
/// skipped entirely because the child happened to `execve` before touching anything).
static LAZY_FAULTS_SERVICED: AtomicUsize = AtomicUsize::new(0);
static LAZY_FAULTS_ZERO_FILLED: AtomicUsize = AtomicUsize::new(0);

/// Child-side: reads [`FORK_CHILD_LAZY_RANGES_ENV_VAR`]/[`FORK_CHILD_PARENT_PID_ENV_VAR`] (set by
/// the parent only when [`lazy_fork_commit_enabled`] and at least one group qualified) and, if
/// present, opens a `PROCESS_VM_READ` handle to the parent and installs [`lazy_commit_veh`] as the
/// FIRST handler in this process' VEH chain (`AddVectoredExceptionHandler(1, ..)` prepends).
///
/// MUST be called before ANY guest code in this child touches a lazily-reserved address --
/// i.e. before `run_thread`/`adopt_forked_process` resumes real guest execution. Call site:
/// `litebox_runner_linux_on_windows_userland`'s fork-child startup path, immediately next to
/// where `FORK_CHILD_VMA_LAYOUT_ENV_VAR` is parsed (same env-var-based bootstrap channel, same
/// "before guest execution resumes" timing guarantee that channel already relies on).
///
/// A no-op (does not touch the VEH chain at all) when the env var is absent/empty -- this is what
/// makes the whole mechanism provably inert for every non-opted-in fork and every ordinary
/// (non-fork) process.
pub fn install_if_configured() {
    let Some(ranges_line) = std::env::var(FORK_CHILD_LAZY_RANGES_ENV_VAR).ok() else {
        return;
    };
    let ranges = deserialize_lazy_ranges(&ranges_line);
    if ranges.is_empty() {
        return;
    }
    let Some(parent_pid) = std::env::var(FORK_CHILD_PARENT_PID_ENV_VAR)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
    else {
        eprintln!(
            "[lazy_fork_commit] {FORK_CHILD_LAZY_RANGES_ENV_VAR} set but {FORK_CHILD_PARENT_PID_ENV_VAR} \
             missing/unparseable -- disabling lazy commit for this child (falling back to whatever \
             the eager path already wrote, which is nothing for these ranges: they will fault as \
             genuine access violations). This should never happen outside a bug in the parent's own \
             spawn_process_fork_child."
        );
        return;
    };
    let parent_handle = unsafe { OpenProcess(PROCESS_VM_READ, 0, parent_pid) };
    if parent_handle.is_null() {
        eprintln!(
            "[lazy_fork_commit] OpenProcess(PROCESS_VM_READ, parent_pid={parent_pid}) FAILED \
             GetLastError={} -- disabling lazy commit for this child",
            unsafe { windows_sys::Win32::Foundation::GetLastError() }
        );
        return;
    }
    PARENT_HANDLE.store(parent_handle as isize, Ordering::SeqCst);
    if std::env::var_os("LITEBOX_DIAG_LAZY_FORK_COMMIT").is_some() {
        eprintln!(
            "[lazy_fork_commit] install_if_configured: registering VEH for {} range(s), parent_pid={parent_pid}, parent_handle={parent_handle:p}: {ranges:?}",
            ranges.len()
        );
    }
    let _ = LAZY_RANGES.set(ranges);
    let installed = unsafe { AddVectoredExceptionHandler(1, Some(lazy_commit_veh)) };
    if std::env::var_os("LITEBOX_DIAG_LAZY_FORK_COMMIT").is_some() {
        eprintln!("[lazy_fork_commit] AddVectoredExceptionHandler returned {installed:p}");
    }
}

/// Vectored Exception Handler: claims an `EXCEPTION_ACCESS_VIOLATION` iff the faulting address
/// falls within a range [`install_if_configured`] registered for THIS process -- every other
/// fault (including every fault this process's own pre-existing `fork_verify`/
/// `vectored_exception_handler` machinery is responsible for) is declined via
/// `EXCEPTION_CONTINUE_SEARCH`, letting it fall through to the next handler in the chain
/// UNCHANGED. This handler never touches `ContextRecord`/registers at all -- it only commits
/// memory and copies bytes, then lets the CPU naturally re-execute the original faulting
/// instruction, which is what makes it safe to run before whatever handler
/// `AddVectoredExceptionHandler(1, ..)` had previously prepended (this process's own
/// `vectored_exception_handler_entry`, if this is a real guest-hosting fork child).
static VEH_ENTRY_COUNT_DIAG: AtomicUsize = AtomicUsize::new(0);

unsafe extern "system" fn lazy_commit_veh(info: *mut EXCEPTION_POINTERS) -> i32 {
    let diag = std::env::var_os("LITEBOX_DIAG_LAZY_FORK_COMMIT").is_some();
    if diag {
        let n = VEH_ENTRY_COUNT_DIAG.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 40 {
            let rec = unsafe { &*(*info).ExceptionRecord };
            let addr = rec.ExceptionInformation.get(1).copied().unwrap_or(0);
            eprintln!(
                "[lazy_fork_commit] VEH entry #{n}: code={:#x} addr={addr:#x} ranges_set={}",
                rec.ExceptionCode,
                LAZY_RANGES.get().is_some()
            );
        }
    }
    let Some(ranges) = LAZY_RANGES.get() else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let rec = unsafe { &*(*info).ExceptionRecord };
    const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC000_0005;
    if rec.ExceptionCode.cast_unsigned() != EXCEPTION_ACCESS_VIOLATION {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let fault_addr = rec.ExceptionInformation[1];
    let Some(_matched) = ranges.iter().find(|r| r.contains(&fault_addr)) else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let page_addr = fault_addr & !(PAGE_SIZE - 1);

    let committed =
        unsafe { VirtualAlloc(page_addr as *mut c_void, PAGE_SIZE, MEM_COMMIT, PAGE_READWRITE) };
    if committed.is_null() {
        // Genuinely can't service this fault (e.g. host OOM) -- decline rather than pretend
        // success; the guest sees a real SIGSEGV via whatever this process's normal unhandled-AV
        // path does, which is the honest outcome when memory cannot be provided.
        return EXCEPTION_CONTINUE_SEARCH;
    }

    let parent_handle = PARENT_HANDLE.load(Ordering::SeqCst) as Handle;
    if !parent_handle.is_null() {
        let mut buf = [0u8; PAGE_SIZE];
        let mut read_len: usize = 0;
        let ok = unsafe {
            ReadProcessMemory(
                parent_handle,
                page_addr as *const c_void,
                buf.as_mut_ptr().cast(),
                PAGE_SIZE,
                &mut read_len,
            )
        };
        if ok != 0 && read_len == PAGE_SIZE {
            unsafe {
                core::ptr::copy_nonoverlapping(buf.as_ptr(), page_addr as *mut u8, PAGE_SIZE);
            }
        } else {
            LAZY_FAULTS_ZERO_FILLED.fetch_add(1, Ordering::Relaxed);
        }
        // else (zero-filled case): unreadable in the parent (real, unmapped padding) -- leave the
        // freshly-committed page zero-filled, matching copy_one_group's own existing behavior for
        // unreadable pages.
    }
    let n = LAZY_FAULTS_SERVICED.fetch_add(1, Ordering::Relaxed) + 1;
    if std::env::var_os("LITEBOX_DIAG_LAZY_FORK_COMMIT").is_some() && n <= 40 {
        eprintln!(
            "[lazy_fork_commit] fault #{n} serviced: page={page_addr:#x} zero_filled_so_far={}",
            LAZY_FAULTS_ZERO_FILLED.load(Ordering::Relaxed)
        );
    }

    EXCEPTION_CONTINUE_EXECUTION
}
