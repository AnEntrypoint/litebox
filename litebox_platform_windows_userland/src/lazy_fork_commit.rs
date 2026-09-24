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
//!
//! # 87th pass -- Bug B/Bug 4 (TOCTOU) design refined into something concretely buildable;
//! **NOT implemented this pass** -- judged too large a correctness surface to land and verify
//! with confidence in one pass without a live `cdb` session, exactly the bar the 86th pass's own
//! Candidate 3 sketch was already close to but left two real gaps in (both closed below). Flag
//! stays default OFF; no runtime behavior changed this pass.
//!
//! Before any design work, re-derived (not assumed) whether the standing "increase the pagefile"
//! idea from the task brief could be a cheaper, orthogonal fix for the RAM crater instead: live
//! `Get-Counter`/`Get-CimInstance Win32_PageFileUsage` on the actual host at pass start showed
//! Commit Limit ~43.9 GB (15.25 GB physical + an automatically-managed ~25.7 GB pagefile) against
//! only ~18.1 GB Committed Bytes -- i.e. ~25 GB of UNUSED commit headroom already exists before
//! any boot attempt even starts. The 76th-82nd passes' own crater measurements (~7-8 GB additional
//! committed at the crater, 28-29 processes) would still land well under the existing commit
//! limit even added on top of the 18.1 GB baseline -- so the crater is real PHYSICAL working-set
//! demand exceeding physical RAM (causing thrashing/eviction), never a Windows commit-limit
//! rejection; growing the pagefile further would not change this, and is a closed angle as of
//! this pass, not merely an unexamined one. (Also re-confirmed the 80th pass's admission-cap
//! finding and the 76th-pass per-process working-set-driven framing both still hold -- nothing in
//! the surrounding system has changed enough since the 86th pass to revisit either without new
//! evidence, which this pass did not find.)
//!
//! **Two real refinements over the 86th pass's own Candidate 3 sketch, found by working through
//! the concurrency argument in full instead of only sketching it:**
//!
//! 1. **The sketch's "global" scope was wrong -- the actual correctness unit is PER-PARENT-PROCESS,
//!    not tree-wide.** Two different parent processes' own fork-guarded relationships touch
//!    disjoint address spaces (each process's own private memory) and can never race each other no
//!    matter how many are concurrently outstanding -- only two overlapping generations from the
//!    SAME parent are the actual hazard the 86th pass identified. A single system-wide "at most one
//!    ever" gate (what the 86th pass's own writeup implied) would be needlessly conservative during
//!    a real boot's fork storm, where many DIFFERENT parent processes (distinct daemons/shells) are
//!    forking concurrently -- exactly the RAM-crater moment this mechanism exists to help. The gate
//!    belongs on ordinary process-LOCAL statics (an `AtomicU32` "current guard-owner child pid, 0 =
//!    free" plus a `Vec<Range<usize>>` of this process's own currently-guarded ranges) -- no shared
//!    arena, no `SharedArc`, no new `SharedKernelStateSlot` variant, no cross-process attach/offset
//!    dance needed for the claim/release decision at all, since only the parent's OWN code ever
//!    decides whether ITS OWN next fork may take the guarded path. The one piece that genuinely
//!    must cross the process boundary -- the captured pre-write snapshot bytes -- can ride the
//!    SAME `PROCESS_VM_READ` handle the child already opens on the parent for [`lazy_commit_veh`]'s
//!    existing live-read fallback (`ReadProcessMemory` doesn't care about a page's protection state,
//!    only that it's committed and not `PAGE_NOACCESS`), reached via a plain `usize` address of the
//!    parent's own snapshot-table static, carried across in ONE new env var at fork time --
//!    symmetric with how [`FORK_CHILD_PARENT_PID_ENV_VAR`] already works, not a new IPC primitive.
//!    Release: a parent, before granting its NEXT fork the guarded path, first does a bounded,
//!    non-blocking liveness check on the recorded owner pid (`OpenProcess(SYNCHRONIZE, ...)` +
//!    `WaitForSingleObject(h, 0)`) and reclaims the slot if that child has since died -- covers
//!    BOTH a fork-without-execve child that eventually exits AND one that crashes, with no explicit
//!    "I'm done" signal needed from the child at all (the tradeoff: a long-lived, successfully
//!    `execve()`'d daemon child holds its parent's slot for the rest of its own life, since nothing
//!    currently distinguishes "still running my own forked code" from "long since `execve()`'d and
//!    now independent" without an active completion signal -- a real, honest limitation flagged for
//!    a future pass to improve, not silently assumed away: it only gives up SOME of the win, never
//!    correctness).
//!
//! 2. **The sketch's "child prefers a recorded snapshot over a live read, if one exists" step has
//!    an unstated TOCTOU of its own -- closed here with an explicit double-checked-state protocol
//!    (a bounded, single-retry seqlock-style read, not a new lock).** A naive "check a `state` flag,
//!    then branch to either the snapshot or a live read" sequence has a gap: the parent's own
//!    write-fault handler could publish the snapshot (flip `state` 0 -> 1) in the window BETWEEN
//!    the child's flag check and its subsequent `ReadProcessMemory` call, in which case that RPM
//!    would observe the parent's POST-write bytes even though the flag it just checked said "no
//!    snapshot yet" -- reintroducing exactly Bug B/Bug 4's torn/stale read, just narrowed to one
//!    smaller window instead of the whole child lifetime. Fix: the child performs its live
//!    `ReadProcessMemory` FIRST (matching today's existing fallback path exactly), then re-checks
//!    the snapshot slot's `state` a SECOND time; if it is now `1` (the parent published a snapshot
//!    at some point during or after the child's read -- indistinguishable from "before", which is
//!    exactly the ambiguity that makes the live read unsound), the child DISCARDS its own live read
//!    and uses the snapshot bytes instead, never the reverse. This is sound because: the parent's
//!    page is `PAGE_READONLY` from fork time until the FIRST write-fault flips `state` to `1` (the
//!    snapshot is captured, published via a `Release` store, while the page is still read-only, so
//!    a write physically cannot have landed before that publish); if the child's post-read re-check
//!    still observes `state == 0`, that is a truthful witness that NO write-fault happened at any
//!    point during the child's own read window (the state transition is monotonic, one-shot, and
//!    globally visible the instant it happens -- there is no way for it to have happened and then
//!    "un-happened" by the time of the re-check), so the live read the child just took is provably
//!    the correct fork-time-through-now value. One retry is always enough (`state` only ever
//!    transitions 0 -> 1, once, for the lifetime of a given guard relationship), so this needs no
//!    loop, no backoff, no new blocking primitive -- two atomic loads and, on the rare
//!    snapshot-wins branch, a fixed 4 KiB copy.
//!
//! **A third, genuinely new finding this pass, NOT present in the 86th pass's own writeup at all**:
//! any parent-side write-fault handler for this mechanism would run as a THIRD entrant into
//! machinery this codebase has already had to fight hard-won, `AGENTS.md`-documented battles over
//! -- `VIRTUAL_PROTECT_LOCK` (`lib.rs`, guards every `VirtualProtect` call against guest-mapped
//! memory process-wide, because two threads racing independent flips of the SAME page, e.g. an
//! ordinary guest `mprotect()` on one thread against [`fork_verify`]'s own AV-path healer on
//! another, produced a real, live `labwc` SIGSEGV before this lock existed) and `fork_verify`'s own
//! `FORK_VERIFY_HEAL_LOCK`-serialized AV-path healing (which can ALSO write-fault-then-heal on
//! guest memory from inside VEH dispatch). A new parent-side handler that `VirtualProtect`s guest
//! memory on a write fault MUST take `VIRTUAL_PROTECT_LOCK` around its own query/flip/write/restore
//! span for the exact reason that lock's own doc comment gives -- checked against precedent
//! (`fork_verify::write_usize_fault_tolerant`, `lib.rs`), the established, ALREADY-SHIPPED pattern
//! for this exact lock is a plain blocking `.lock()` even from inside VEH dispatch (not the
//! try-lock-plus-bounded-spin pattern `FORK_VERIFY_HEAL_LOCK` itself uses for a much LARGER
//! critical section) -- i.e. this is a solved problem in this codebase already, not a new one, as
//! long as a future implementation follows that exact precedent rather than inventing a new
//! locking discipline. Flagged explicitly because getting this wrong (e.g. reaching for `.lock()`
//! inside a handler that can itself be re-entered on the same thread across a larger span) is
//! exactly the class of subtle regression `AGENTS.md`'s own 84th-pass entry warns future
//! `vectored_exception_handler`-adjacent work about.
//!
//! **Why this pass stops at design, not code**: even with both gaps closed and the locking
//! precedent identified, this is a new VEH firing on ordinary, ONGOING write traffic from an
//! ALREADY-RUNNING, potentially long-lived guest process (unlike [`lazy_commit_veh`], which only
//! ever runs during a fresh fork child's own early startup, before most of the codebase's other
//! fragile machinery is even relevant yet) -- a bug here risks corrupting or crashing a process the
//! rest of the boot already depends on (e.g. `dbus-daemon` itself), not just an isolated,
//! easily-rerun fork-child repro. Landing it needs the same live `cdb`/diagnostic-gated
//! verification budget the 83rd-85th passes each spent a full pass on for a SMALLER mechanism, plus
//! a genuinely new concurrent-fork repro (two children racing one mutating parent) this codebase
//! does not yet have. Shipping it under-verified would trade today's honest, deterministic Bug
//! B/Bug 4 crash for a silently-wrong-data risk on a live daemon -- exactly what the standing
//! project discipline forbids. **Concrete pickup for the next pass with a live-debug budget**:
//! implement exactly the design above (parent-local claim/release statics; one new env var
//! carrying the parent's own snapshot-table address; the double-checked-state child read; a
//! `VIRTUAL_PROTECT_LOCK`-guarded parent write-fault VEH modeled on
//! `fork_verify::write_usize_fault_tolerant`'s own locking), gated behind a SEPARATE, additional,
//! default-OFF flag (e.g. `LITEBOX_LAZY_FORK_GUARD_COW=1`, checked in addition to
//! [`lazy_fork_commit_enabled`]) so it cannot change anything about the already-working
//! fork-then-`execve` path even if buggy; verify 5/5 on both existing repros (regression check with
//! the new flag OFF; the fix check with it ON) PLUS a new concurrent-two-children-one-parent repro,
//! both debug and release, before ever considering flipping either flag on for a real boot.
//!
//! # 88th pass -- IMPLEMENTED, gated behind `LITEBOX_LAZY_FORK_GUARD_COW=1` (additional to
//! `LITEBOX_LAZY_FORK_COMMIT=1`), narrowly scoped to the single-outstanding-child case the design
//! above is provably sound for. **Both flags stay default OFF; `LITEBOX_LAZY_FORK_COMMIT=1` alone
//! is unchanged** (guard-cow code paths are only reachable when the new env var is also set).
//!
//! Implementation matches the 87th pass's design exactly: a process-local single-owner slot
//! ([`GUARD_COW_OWNER_PID`], claimed via [`try_claim_guard_cow_for_fork`] with a CAS placeholder
//! plus a bounded liveness reclaim -- `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`+
//! `GetExitCodeProcess`, not the 87th pass's own sketched `SYNCHRONIZE`/`WaitForSingleObject`,
//! switched during implementation to reuse `WindowsUserland::is_process_alive`'s (`lib.rs`)
//! already-shipped idiom verbatim rather than introduce a second liveness-check shape) gates
//! whether a NEW fork may use guard-cow at all -- if another
//! child from the SAME parent is still alive and holding the slot, this fork gets ZERO lazy groups
//! (forced full eager fallback for every group, not merely "plain lazy" -- the conservative choice
//! the task brief itself asked for), never a mix of guarded and unguarded lazy groups. When the
//! claim succeeds, every lazy-eligible group for that one fork is BOTH lazily reserved in the child
//! (as before) AND `VIRTUAL_PROTECT_LOCK`-guarded to `PAGE_READONLY` in the PARENT's own already-
//! committed pages ([`GuardCowState`]/[`GuardedRegion`], walked per-`VirtualQuery`-uniform
//! sub-region so a mixed-protection group is handled correctly), with a `'static`-leaked
//! [`GuardSnapshotSlot`] table (one slot per guest page across every guarded group, `state: AtomicU8`
//! + `bytes: UnsafeCell<[u8; 4096]>`, `#[repr(C)]` so both processes -- literally the same compiled
//! binary, `current_exe()`-respawned -- agree on layout with no hand-computed offsets, using
//! `core::mem::offset_of!` instead) reachable by the child through one new env var
//! ([`FORK_CHILD_GUARD_COW_TABLE_ENV_VAR`], a plain `usize` address) over the SAME
//! `PROCESS_VM_READ` handle [`lazy_commit_veh`] already opens on the parent -- no new IPC
//! primitive. [`guard_cow_write_fault_veh`] is installed (once, lazily, `OnceLock`-guarded) on the
//! PARENT the first time it ever grants a guarded fork; it declines (`EXCEPTION_CONTINUE_SEARCH`)
//! every fault that is not a WRITE to a currently-guarded page, and for one that is, takes
//! `VIRTUAL_PROTECT_LOCK` for the whole query-flip-copy-restore span (the exact precedent
//! `fork_verify::write_usize_fault_tolerant` already established for this lock from inside VEH
//! dispatch), snapshots the page's pre-write bytes into its slot, publishes `state=1` with
//! `Ordering::Release`, restores the region's own recorded `old_protect` (never a blanket
//! `PAGE_READWRITE` -- a sub-range that was already read-only in the guest keeps that meaning
//! after healing), and returns `EXCEPTION_CONTINUE_EXECUTION`. [`lazy_commit_veh`] implements the
//! double-checked-state protocol exactly as designed: live `ReadProcessMemory` FIRST, THEN a
//! second, separate `ReadProcessMemory` of the slot's `state` byte -- if that post-read check now
//! reads `1`, the live read is DISCARDED and a fresh `ReadProcessMemory` of the slot's `bytes`
//! is used instead, never the reverse (see the 87th pass's own proof of why this ordering, not
//! "check-then-branch", is the one that is actually race-free).
//!
//! **Verification this pass (both debug and release, all real command executions with real
//! stdout/exit-code/`LITEBOX_DIAG_LAZY_FORK_COMMIT=1`/`LITEBOX_DIAG_FORK_TIMING=1` evidence, no
//! cdb attach needed for any of it)** -- see `AGENTS.md`'s 88th-pass entry for the exact commands
//! and full output. Summary, all real: `LITEBOX_PROCESS_FORK=1` alone (both new flags unset)
//! reconfirmed byte-identical (hello/done, exit 0). The fork-then-execve repro is clean 5/5 both
//! builds with both flags on, AND the log shows the mechanism genuinely engaging mid-run (real
//! `parent write-fault captured` / `snapshot preferred over live read (post-read state==1)`
//! lines) -- not a no-op. The single-subshell repro (the one Bug 4 was originally found on,
//! previously 5/5 killed under `LAZY_FORK_COMMIT=1` alone, reconfirmed still 5/5 killed this pass
//! with the SAME documented `malloc.c:2601` signature when guard-cow is off) is clean 5/5, both
//! builds, `subshell_child`/`inner_var`/`parent_after` all print, exit 0, under
//! `LAZY_FORK_COMMIT=1 LAZY_FORK_GUARD_COW=1`. A new two-overlapping-forks-one-parent repro
//! (`(sleep 1; ...) & (echo B_start; ...) & wait`) confirms, via the exact predicted
//! `reserve_group_lazy`/`copy_one_group` call counts (2 lazy + 16 eager across 3 total forks: the
//! first child gets guard-cow, the SECOND, overlapping child gets zero lazy groups at all -- forced
//! fully eager -- and the first child's own later nested fork of `/bin/sleep` gets its own
//! independent grant, its own process-local slot, exactly as the per-parent-process design
//! intends) that the single-owner gate works as designed under real overlap, with correct output
//! from every child. One honest, precisely-measured caveat, NOT a correctness concern: for this
//! specific repro's real guest layout, only 1 of 6 fork-carried groups is lazy-eligible at all
//! (the other 5 are large-and/or-`VM_EXEC`), so the per-fork group-copy-phase timing difference
//! between eager-only (~72ms), plain-lazy (~70.5ms) and guard-cow (~95.8ms, extra `VirtualQuery`/
//! `VirtualProtect` bookkeeping over that one small group) is small relative to noise and NOT the
//! large win the 83rd pass measured on its own, differently-shaped repro -- guard-cow's real cost
//! center is the PARENT's own later write-fault handling (16 captures observed live in the overlap
//! repro), which is bounded, page-granular, self-healing, and orthogonal to this specific number.
//!
//! **Bug 5 (found via a REAL boot attempt, not an isolated repro -- FIXED, live-verified): a
//! superseded claim's guarded regions were never healed before the NEXT claim could re-guard the
//! SAME address range, producing an infinite same-instruction re-fault loop under a real,
//! longer-lived, multi-fork parent.** All four isolated repros above passed clean the moment they
//! were written -- but this pass did not stop there and attempted the actual
//! `de_only_xcensus_seed3.tar` boot per the task brief's own item 8, which is what surfaced this:
//! the root process accumulated 150+ CPU-seconds with the process tree stuck at exactly 2
//! processes and ZERO further `DIAG_TIMELINE` progress past the second fork, while a live cross-
//! process child sat almost perfectly idle (0.03s CPU) -- a signature inconsistent with a genuine
//! block/deadlock (which would show near-0% CPU) and inconsistent with legitimate work (which
//! would show forward progress). A/B confirmed it was guard-cow-specific: the identical boot with
//! `LITEBOX_LAZY_FORK_GUARD_COW` unset stayed low-CPU and exited on its own by ~30s (hitting the
//! ALREADY-DOCUMENTED Bug 4 heap corruption repeatedly instead -- itself useful confirmation that
//! Bug 4 is real on a genuine multi-fork boot, not just the isolated subshell repro). Root cause,
//! found by re-reading [`reserve_group_lazy_guarded`]'s own protect-walk against the claim
//! lifecycle rather than guessing: when [`try_claim_guard_cow_for_fork`] reclaims a slot from a
//! DEAD former owner, the dead owner's [`GuardCowState`] regions were left exactly as they were --
//! still `PAGE_READONLY` if the parent had simply never gotten around to writing to them before
//! that child died (the common case for a short-lived `mkdir`-style fork-then-execve child: it
//! exits before the parent's OWN continued execution ever revisits that address range). The next
//! claim's own [`reserve_group_lazy_guarded`] then `VirtualProtect(..., PAGE_READONLY, &mut
//! old_protect)`s the SAME still-protected range -- and Win32's own `old_protect` out-parameter
//! faithfully reports the CURRENT protection (already `PAGE_READONLY`), not the true value from
//! before ANY guarding ever happened. That poisoned `old_protect` is what
//! [`guard_cow_write_fault_veh`] later "restores" to on the parent's real first write -- a
//! no-op, since the page was already exactly that value -- so `EXCEPTION_CONTINUE_EXECUTION`
//! re-executes the SAME faulting store, which re-faults instantly, forever: real CPU burned on
//! every iteration's full exception dispatch (fault delivery, lock acquisition, region lookup),
//! no crash, no progress, indistinguishable from outside the process from a hang. **Fix**:
//! [`try_claim_guard_cow_for_fork`]'s dead-owner-reclaim branch now calls
//! [`heal_superseded_guard_state`] -- under the SAME `GUARD_STATE` -> `VIRTUAL_PROTECT_LOCK` lock
//! order [`guard_cow_write_fault_veh`] itself uses (so no other thread's write fault can ever
//! observe `GUARD_STATE` as `None` while a healable region is mid-restore), restoring every
//! region the dead claim ever guard-protected back to ITS OWN recorded `old_protect` -- captured
//! back when that region was genuinely never-before-guarded -- BEFORE the reclaiming fork's own
//! protect walk can run. **Verified live**: a cheap, targeted repro exercising exactly this
//! reclaim sequence (`bash -c 'mkdir -p /tmp/a; mkdir -p /tmp/b; ...` x8, sequential fork-then-
//! execve from one long-lived parent, each child dying before the parent revisits that memory --
//! the exact shape the real boot's own `mkdir`/`mkdir` pair hit) hung before this fix and
//! completes cleanly (`SEQ_DONE` printed, all 8 forks' own `task-resume-probe` lines present, zero
//! corruption) after it, both debug and release. The four original repros above were ALL
//! re-verified 5/5 clean, both builds, after this fix landed (unchanged from before it -- this fix
//! only touches the dead-owner-reclaim path those four repros never exercised, since none of them
//! has a THIRD fork reclaiming a SECOND dead claim's slot).
//!
//! **Real `de_only_xcensus_seed3.tar` boot result after Bug 5's fix, both `LITEBOX_LAZY_FORK_
//! COMMIT=1 LITEBOX_LAZY_FORK_GUARD_COW=1`**: ran the full ~195s monitoring window WITHOUT ever
//! cratering and WITHOUT hanging -- free RAM oscillated in a stable 2.8-4.5GB band (process count
//! 9-16) for the entire window, a qualitatively different, far healthier trajectory than every
//! prior pass's own documented crater (28-29 processes, <1GB free). Real forward progress reached:
//! `DE_ONLY_START` -> `XSOCK_WAIT_DONE` -> `DBUS_UP` -> `DE_LAUNCHED_DIRECT` -> `WM_POLL` n=1..12
//! -> `XCENSUS_WINDOWS total=1` (a real X window exists) -> the SAME already-documented
//! `DE_FAILED after 60s` (`_NET_SUPPORTING_WM_CHECK` never appearing -- AGENTS.md's own
//! longstanding, separate, not-yet-root-caused xfwm4 registration gap, Track B item unrelated to
//! this mechanism). **`DE_UP` was NOT reached this pass** -- the presenting blocker at the moment
//! of failure was the pre-existing WM-registration gap, not RAM/process exhaustion, which is
//! itself the real, measured, positive result: for this specific harness and this specific run,
//! the RAM-crater blocker this whole 76th-88th-pass investigation exists to fix was not what
//! stopped the boot. One run is not five; re-verifying 5/5 on the real boot (expensive, ~3+
//! minutes each) was judged lower value than landing and documenting Bug 5's fix and this one
//! clean data point within this pass's own remaining budget -- a concrete pickup for the next
//! pass, alongside root-causing the pre-existing `DE_FAILED`/`_NET_SUPPORTING_WM_CHECK` gap now
//! that RAM is no longer in the way of reaching it.
//!
//! # 89th pass -- generalized guard-cow from one outstanding child per parent to ANY number of
//! concurrent generations, by re-deriving the correctness unit from first principles rather than
//! literally implementing the 87th/88th passes' own "N shadow slots" sketch.
//!
//! **The key simplification, found by working the proof through rather than assuming the sketch's
//! own framing**: the 87th/88th passes' own writeup imagined needing per-page MULTIPLE VERSIONED
//! snapshots (one per generation, tagged by fork time, so a later child doesn't get served an
//! earlier one's stale pre-write copy). That framing is unnecessarily conservative. The real
//! invariant a page's `PAGE_READONLY` protection provides is much stronger: for as long as a page
//! stays guarded, NO write has landed on it by construction (a write would have hardware-faulted
//! and been serviced) -- so EVERY generation that forks while a page is ALREADY open (guarded, at
//! least one other live generation already pending on it) is, by definition, forking during the
//! exact same unbroken no-write span as whichever generation opened it. There is therefore only
//! ONE live value in play for the whole span, not one per generation: when the eventual write
//! finally comes, a SINGLE captured snapshot correctly serves every currently-pending generation
//! at once, because none of them could possibly have a different "correct as of my fork" answer.
//! This means the real per-page state needed is not "N tagged shadow versions" but "one open
//! interval, with a growing/shrinking SET of generations currently relying on it" -- closed
//! (snapshot distributed to everyone pending, page reopened for writing) the instant a write
//! occurs, and reopened fresh (empty set, real current `VirtualQuery`d protection recorded) the
//! next time any generation wants to guard that address again. No versioning, no generation
//! timestamps, no multi-slot-per-page table needed at all.
//!
//! **Architecture**: [`GUARD_PAGE_REGISTRY`] (`Mutex<Option<HashMap<usize, PageGuardEntry>>>`),
//! keyed by page-aligned address, replaces the 88th pass's single owner-pid gate + one-claim-at-a-
//! time `GUARD_STATE`. Deliberately an ordinary process-local `Mutex`, NOT a shared-arena/
//! `SharedArc` structure -- re-derived, not assumed, from the 87th pass's own finding 1 (still
//! true, unaffected by this generalization): every reader and writer of this state is a thread of
//! the SAME parent process (the guest thread(s) issuing `fork()`, and this process's own
//! `guard_cow_write_fault_veh` reacting to ITS OWN write faults). A lazy CHILD never touches this
//! map directly -- it only ever `ReadProcessMemory`s a specific [`GuardSnapshotSlot`]'s
//! bytes/state, exactly as the 88th pass already had it, over the SAME `PROCESS_VM_READ` handle.
//! Each fork's own [`GuardCowClaim`] still gets its own freshly `Box::leak`ed snapshot table
//! (unchanged, same accepted leak-per-successful-guarded-fork tradeoff the 88th pass already
//! documented) -- what is NEW is that a single physical page's [`PageGuardEntry`] `pending` list
//! can hold [`PendingGeneration`] entries whose `slot` pointers point into DIFFERENT claims'
//! DIFFERENT tables, one per currently-interested fork.
//!
//! **Liveness is now tracked by a kept-open `HANDLE`, not a re-resolved `u32` pid.** The 88th
//! pass's liveness check re-opened the pid fresh on every check -- sound for a single, short-lived
//! claim/reclaim decision, but this generalization can leave a generation "pending" for
//! arbitrarily long (as long as the page stays unwritten), during which its owning pid could exit
//! AND be reused by an unrelated later process. [`PendingGeneration`] instead keeps its own
//! `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` handle, taken at the moment it joins a page,
//! and checks liveness through THAT handle ([`pending_generation_alive`]) -- immune to pid reuse
//! for as long as the entry exists, at the cost of one extra open handle per (generation, guarded
//! page) pair for the life of that pairing (closed via `PendingGeneration`'s own `Drop`, run
//! whenever an entry is pruned, serviced, or a claim aborts).
//!
//! **The single global one-at-a-time gate is gone, but a bounded N-at-a-time gate replaced it
//! (102nd pass).** Originally this comment claimed there was "no longer a reason to force a whole
//! fork back to eager just because another sibling is outstanding" -- the 99th-101st passes' own
//! real-boot A/B evidence (a crater-speed regression, craters in ~14-21s at 7 processes vs. the
//! pre-98th-pass code's documented stable 195-300s window at 9-16 processes, NOT explained by the
//! 101st pass's own ruled-out per-page `VirtualProtect` theory) refutes that: the `6`-wide
//! `live_cross_process_fork_children` cap bounds concurrent CHILDREN, but each concurrent child now
//! ALSO leaks its own full-group-sized [`GuardSnapshotSlot`] table (unconditional, unchanged since
//! the 88th pass), so removing the old 1-at-a-time gate let a busy parent accumulate up to 6x as
//! much simultaneously-live leaked parent-side commit as the old code ever could -- see
//! [`GUARD_COW_OPEN_CLAIMS`]'s own doc comment for the full derivation. [`try_claim_guard_cow_table`]
//! now declines (falls the whole fork back to eager, exactly like the 88th pass's gate did for ANY
//! overlap) once [`GUARD_COW_CONCURRENT_CLAIM_CAP`] claims are simultaneously open, not just for the
//! degenerate `total_pages == 0` case. Per-fork admission is ALSO still bounded upstream, for free,
//! by `litebox_shim_linux::GlobalState`'s own existing `live_cross_process_fork_children` cap (6
//! concurrent cross-process-fork children system-wide, 76th pass) -- the two caps are independent
//! and both apply; this module's own cap is the tighter, more targeted one for the specific
//! leaked-table-commit cost this doc comment describes.
//!
//! **Per-page join/open logic ([`guard_one_page`]) holds [`GUARD_PAGE_REGISTRY`]'s lock for its
//! ENTIRE decide-then-act body**, including the nested [`crate::VIRTUAL_PROTECT_LOCK`] acquisition
//! for a fresh guard's own `VirtualProtect` call (lock order: registry, then virtual-protect,
//! matching [`guard_cow_write_fault_veh`]'s own order, so the two paths can never deadlock against
//! each other) -- closes a race an earlier draft of this design had (drop the registry lock between
//! "decide fresh vs join" and "act on that decision", during which a second concurrently-forking
//! guest thread on the same parent could open or close the SAME page's interval and have its own
//! bookkeeping silently overwritten). Caught by re-deriving the concurrency argument in full before
//! writing the fault-handler code, not found live -- the discipline this project's own 84th-pass
//! entry asks future `vectored_exception_handler`-adjacent work to apply.
//!
//! **Dead-generation pruning is now per-PAGE, checked on every join attempt** ([`guard_one_page`]'s
//! own `entry.pending.retain(pending_generation_alive)` step) -- the direct generalization of the
//! 88th pass's Bug 5 fix (which pruned per-CLAIM, only on a single-slot reclaim). A page whose only
//! pending generations have since died is, for a NEW joiner, exactly as fresh as a page with no
//! entry at all; pruning (rather than tearing the whole entry down) preserves the recorded
//! `true_original_protect` so the newcomer does not need to pay a redundant `VirtualQuery`.
//!
//! **The write-fault handler ([`guard_cow_write_fault_veh`]) services every still-alive pending
//! generation for the faulting page in one pass** (`entry.pending.drain(..)`, one live
//! `copy_nonoverlapping` of the page's current bytes shared across all of them, since -- per the
//! key simplification above -- they all want the identical value), then restores the page's real
//! prior protection and leaves the map entry present but empty (never removed outright). Leaving
//! an emptied entry in place, rather than deleting it, is deliberate: it is what makes a genuinely
//! concurrent SECOND write fault on the very same page (both threads faulted before either one's
//! restore took effect -- physically rare but not impossible on real multi-core hardware) resolve
//! correctly. The second thread's own VEH invocation, arriving after the first has already
//! serviced-and-restored under the same lock, finds the entry still there with an already-empty
//! `pending` list -- skips capture (nothing left to distribute) and simply re-asserts the same
//! `VirtualProtect` restore (idempotent, already at that value) before returning
//! `EXCEPTION_CONTINUE_EXECUTION` for ITS OWN retry too. Deleting the entry instead would have left
//! that second thread's fault matching nothing, falling through to `EXCEPTION_CONTINUE_SEARCH`
//! with no other handler able to resolve it -- a latent crash this design closes by construction
//! rather than by observed failure. The cost is a bounded, harmless leak of empty
//! `PageGuardEntry` records for every distinct page ever guarded over a process's whole lifetime
//! (tens of bytes each) -- accepted, consistent with this module's other already-documented
//! per-fork leaks (leaked snapshot tables), and self-evidently bounded by the guest's own total
//! distinct-page count, not by fork count.
//!
//! **101st pass: the per-page `VirtualProtect` cost flagged below by the 89th pass is now batched**
//! ([`try_guard_region_batched`], called first by [`reserve_group_lazy_guarded`]'s inner loop):
//! when an entire `VirtualQuery`-uniform sub-range has NO existing [`GUARD_PAGE_REGISTRY`] entry at
//! all (checked under one lock acquisition), it is guarded with ONE `VirtualProtect` call across
//! the whole sub-range instead of one call per page. [`guard_one_page`]'s original per-page path is
//! UNCHANGED and still runs, unmodified, as the fallback the instant any page in a sub-range
//! already has a live entry (a genuine overlap between two concurrently-forking generations from
//! the same parent) -- so the join-vs-open decision's own correctness argument is untouched; the
//! batching only ever takes over the strict subset of cases that would have gone through the
//! per-page "open fresh interval" branch for every page in the sub-range anyway (same `old_protect`
//! for all of them by construction, since a `VirtualQuery` region is protection-uniform). Verified
//! this pass on the isolated fork-then-execve and fork-without-execve subshell repros (both flags
//! on, both debug and release) -- see `AGENTS.md`'s 101st-pass entry for exact commands/output; a
//! real-boot timing A/B against the pre-batch binary is this pass's own next step, not yet
//! consolidated into this comment at the time it was written.
//!
//! **Verification this pass**: see this project's own `AGENTS.md` 98th-pass entry for the exact
//! commands and real output -- both original repros (fork-then-execve; fork-without-execve
//! subshell), a NEW concurrent-multi-child repro this generalization exists to cover, and the real
//! `de_only_xcensus_seed3.tar` boot, all re-verified against this design before it is trusted.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicIsize, AtomicU8, AtomicU32, AtomicUsize, Ordering};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
    EXCEPTION_POINTERS, ReadProcessMemory,
};
use windows_sys::Win32::System::Memory::{
    MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER, MEM_EXTENDED_PARAMETER_0,
    MEM_EXTENDED_PARAMETER_1, MEM_RESERVE, MEMORY_BASIC_INFORMATION,
    MemExtendedParameterAddressRequirements, PAGE_GUARD, PAGE_NOACCESS, PAGE_READONLY,
    PAGE_READWRITE, VirtualAlloc, VirtualAlloc2, VirtualProtect, VirtualQuery,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};

/// Windows' `STILL_ACTIVE` sentinel (`GetExitCodeProcess`'s "exit code" for a process that has
/// not exited) -- not exposed by `windows_sys`; same local const `WindowsUserland::is_process_alive`
/// (`lib.rs`) already defines for the identical reason.
const STILL_ACTIVE: u32 = 259;

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

/// Internal-only marker env var: the PARENT's own guard-cow snapshot table base address (a plain
/// `usize`, decimal), set only when [`try_claim_guard_cow_for_fork`] succeeded for this fork --
/// see the module doc comment's "88th pass" section. The child derives each guarded page's slot
/// index locally from [`FORK_CHILD_LAZY_RANGES_ENV_VAR`]'s own ordering (guard-cow guards exactly
/// the ranges that env var already carries, 1:1, same order both sides -- no separate range list
/// needs to cross the process boundary). Never guest-visible.
pub const FORK_CHILD_GUARD_COW_TABLE_ENV_VAR: &str = "LITEBOX_INTERNAL_FORK_CHILD_GUARD_COW_TABLE";

/// Opt-in flag, ADDITIONAL to [`lazy_fork_commit_enabled`]: `LITEBOX_LAZY_FORK_GUARD_COW=1`
/// enables the single-generation guard-page software-COW mechanism (88th pass) that closes Bug
/// B/Bug 4's TOCTOU for the case it is provably sound for -- at most one outstanding (fork-time to
/// fully-independent) lazy-tracked child per parent at a time. Unset (the default) leaves
/// [`lazy_fork_commit_enabled`]'s own existing plain-lazy behavior completely unchanged, even with
/// `LITEBOX_LAZY_FORK_COMMIT=1` set.
#[must_use]
pub fn guard_cow_enabled() -> bool {
    std::env::var_os("LITEBOX_LAZY_FORK_GUARD_COW").is_some()
}

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

/// **Bug 7 (found 100th pass, FIXED here) -- the real root cause of the 787b139/99th-pass
/// regression's `XCENSUS_PRE_DE rc=134 corrupted size vs. prev_size` heap corruption.**
///
/// [`install_if_configured`]'s own doc comment already states the design invariant: [`LAZY_RANGES`]/
/// [`PARENT_HANDLE`] are "written exactly ONCE... before the child resumes any guest execution, and
/// never mutated again". That was always true for a fork child that never calls `execve` again --
/// but `sys_execve` (`litebox_shim_linux::syscalls::process`) does NOT spawn a new Windows process;
/// it tears down and reloads the CURRENT process's own guest image IN PLACE (`release_memory` then
/// `load_program`, same PID, same host process, see that function's own "After this point, the old
/// program is torn down" comment). [`lazy_commit_veh`] is installed once via
/// `AddVectoredExceptionHandler` and is NEVER removed by `execve` -- nothing about reloading the
/// guest image touches the VEH chain. So a process that was EVER a lazy-fork child (even one whose
/// own group ranges were only ever `MEM_RESERVE`d, never actually touched/committed before it
/// `execve`'d) keeps this handler armed, with the SAME stale [`LAZY_RANGES`] addresses and the SAME
/// stale [`PARENT_HANDLE`], for the rest of that Windows process's ENTIRE remaining lifetime --
/// including across arbitrarily many FUTURE `execve()` calls into completely unrelated programs
/// (`sh` -> `cpp` -> `cc1`, the exact real-boot chain that surfaced this: `xrdb` forks a lazy child,
/// which `execve`s `/bin/sh`, which forks its OWN lazy child which `execve`s `cpp`, etc. -- every
/// one of those `execve`s left the ORIGINAL fork's `lazy_commit_veh`/`LAZY_RANGES` fully armed).
///
/// A freshly `execve`'d program's own allocator is highly likely to reuse the SAME address range
/// its predecessor's memory just occupied (Windows' free-region search naturally prefers a range
/// just freed/reserved by this same process's own immediately-preceding `release_memory`/
/// `deallocate_pages` calls) -- so the NEW program's own heap/stack can genuinely fault inside an
/// OLD, stale [`LAZY_RANGES`] entry. When that happens, [`lazy_commit_veh`] "helpfully" services the
/// fault by `ReadProcessMemory`-ing the ORIGINAL parent (a process the new program has zero logical
/// relationship to) and copying THAT unrelated data into the new program's fresh page -- silently
/// seeding the new program's heap/stack with garbage instead of a clean/zero page, exactly the shape
/// a `malloc_state`/chunk-header consistency assertion (`corrupted size vs. prev_size`) would catch.
/// This is a different, independent bug from Bug 6a/6b above (those are about the PARENT-side
/// `GUARD_PAGE_REGISTRY` desyncing from ordinary guest `mprotect`/`munmap` -- this is about the
/// CHILD-side lazy-population VEH surviving past the point its own tracked ranges stop meaning
/// anything at all).
///
/// **Fix**: this flag, checked FIRST in [`lazy_commit_veh`] before even consulting [`LAZY_RANGES`].
/// [`disarm_on_execve`] sets it and closes [`PARENT_HANDLE`] -- called from `sys_execve`'s own
/// existing `end_fork_child_verification()` hook (`litebox_shim_linux::syscalls::process`, already
/// called at exactly "the old program is torn down" point for the UNRELATED thread-based
/// `fork_verify` mechanism's own teardown; `WindowsUserland::end_fork_child_verification`,
/// `lib.rs`, now also calls this). `OnceLock`s ([`LAZY_RANGES`]/[`GUARD_GROUP_BASES`]) cannot
/// themselves be reset on stable Rust, so this flag is the gate instead -- functionally equivalent
/// (every reader of those statics goes through [`lazy_commit_veh`], which now refuses to reach them
/// at all once disarmed) without needing to change their storage type. A no-op, one relaxed atomic
/// load, for the overwhelming majority of VEH invocations that belong to an entirely unrelated fault
/// class (this process's own main `vectored_exception_handler_entry`/`fork_verify` machinery).
static DISARMED_BY_EXECVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Called from `sys_execve`'s teardown (via `WindowsUserland::end_fork_child_verification`) to
/// permanently disarm this process's own lazy-fork-commit child-side machinery -- see
/// [`DISARMED_BY_EXECVE`]'s own doc comment (Bug 7, 100th pass) for why this is necessary and safe.
/// Idempotent and cheap to call even when this process was never a lazy fork child at all (the
/// common case for most `execve`s): [`PARENT_HANDLE`] is simply `0` already, so the `CloseHandle`
/// branch is skipped.
pub fn disarm_on_execve() {
    DISARMED_BY_EXECVE.store(true, Ordering::SeqCst);
    let old = PARENT_HANDLE.swap(0, Ordering::SeqCst) as Handle;
    if !old.is_null() {
        unsafe { CloseHandle(old) };
    }
}

/// Child-side guard-cow state (88th pass), populated at most once, alongside [`LAZY_RANGES`], by
/// [`install_if_configured`]. `0` means "no guard-cow table for this fork" -- either the flag was
/// off, or [`try_claim_guard_cow_for_fork`] declined this fork's claim (another child from the
/// same parent was still outstanding), in which case this fork has ZERO lazy groups at all (see
/// `spawn_process_fork_child`'s own caller-side fallback) and this table is simply never consulted.
static GUARD_TABLE_BASE: AtomicUsize = AtomicUsize::new(0);
/// Child-side: `(group range, this group's own first page's slot index)` pairs, in the SAME order
/// [`FORK_CHILD_LAZY_RANGES_ENV_VAR`] carried them -- lets [`lazy_commit_veh`] compute a fault
/// address's slot index with no extra IPC (the parent computes the identical mapping when sizing
/// and populating the table, `reserve_group_lazy_guarded`'s own `group_slot_base` parameter).
static GUARD_GROUP_BASES: OnceLock<Vec<(Range<usize>, usize)>> = OnceLock::new();

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
    // Guard-cow (88th pass): only present when `try_claim_guard_cow_for_fork` succeeded for THIS
    // fork in the parent -- see `FORK_CHILD_GUARD_COW_TABLE_ENV_VAR`'s own doc comment. Builds the
    // SAME `(range, slot_base)` mapping the parent used when sizing/populating the table, from the
    // SAME ordered `ranges` list, so both sides agree with no further IPC.
    if let Some(table_base) = std::env::var(FORK_CHILD_GUARD_COW_TABLE_ENV_VAR)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        && table_base != 0
    {
        let mut bases = Vec::with_capacity(ranges.len());
        let mut next_slot = 0usize;
        for r in &ranges {
            bases.push((r.clone(), next_slot));
            next_slot += r.len().div_ceil(PAGE_SIZE);
        }
        let _ = GUARD_GROUP_BASES.set(bases);
        GUARD_TABLE_BASE.store(table_base, Ordering::SeqCst);
        if std::env::var_os("LITEBOX_DIAG_LAZY_FORK_COMMIT").is_some() {
            eprintln!(
                "[lazy_fork_commit] install_if_configured: guard-cow table_base={table_base:#x}"
            );
        }
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
    // Bug 7 (100th pass) -- see [`DISARMED_BY_EXECVE`]'s own doc comment. Checked FIRST, before
    // even `LAZY_RANGES`: once this process has `execve`'d, its old fork-time ranges no longer
    // mean anything about the CURRENT guest image, and must never be used to service a fault.
    if DISARMED_BY_EXECVE.load(Ordering::SeqCst) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
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
        let live_ok = unsafe {
            ReadProcessMemory(
                parent_handle,
                page_addr as *const c_void,
                buf.as_mut_ptr().cast(),
                PAGE_SIZE,
                &mut read_len,
            )
        } != 0
            && read_len == PAGE_SIZE;

        // Guard-cow double-checked-state read (88th pass; see this module's own doc comment,
        // "87th pass" design refinement 2, for the correctness argument): only reachable when
        // `install_if_configured` found a guard-cow table for THIS fork
        // ([`GUARD_GROUP_BASES`] non-empty). Deliberately does the live read FIRST (above), then
        // re-checks the snapshot slot's `state` byte SECOND -- if it now reads 1, the live read
        // just taken is discarded (it cannot be proven to predate the parent's write) and the
        // published snapshot is used instead.
        let mut used_snapshot = false;
        if let Some(bases) = GUARD_GROUP_BASES.get()
            && let table_base = GUARD_TABLE_BASE.load(Ordering::SeqCst)
            && table_base != 0
            && let Some((grange, gbase)) = bases.iter().find(|(r, _)| r.contains(&page_addr))
        {
            let slot_index = gbase + (page_addr - grange.start) / PAGE_SIZE;
            let slot_addr = table_base + slot_index * core::mem::size_of::<GuardSnapshotSlot>();
            let state_offset = core::mem::offset_of!(GuardSnapshotSlot, state);
            let bytes_offset = core::mem::offset_of!(GuardSnapshotSlot, bytes);
            let mut state_byte = [0u8; 1];
            let mut sl: usize = 0;
            let state_ok = unsafe {
                ReadProcessMemory(
                    parent_handle,
                    (slot_addr + state_offset) as *const c_void,
                    state_byte.as_mut_ptr().cast(),
                    1,
                    &mut sl,
                )
            } != 0
                && sl == 1;
            if state_ok && state_byte[0] == 1 {
                let mut snap_len: usize = 0;
                let snap_ok = unsafe {
                    ReadProcessMemory(
                        parent_handle,
                        (slot_addr + bytes_offset) as *const c_void,
                        buf.as_mut_ptr().cast(),
                        PAGE_SIZE,
                        &mut snap_len,
                    )
                } != 0
                    && snap_len == PAGE_SIZE;
                if snap_ok {
                    used_snapshot = true;
                    if std::env::var_os("LITEBOX_DIAG_LAZY_FORK_COMMIT").is_some() {
                        eprintln!(
                            "[lazy_fork_commit] guard-cow: page={page_addr:#x} slot={slot_index} \
                             snapshot preferred over live read (post-read state==1)"
                        );
                    }
                }
            }
        }

        if used_snapshot || live_ok {
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

// ============================================================================================
// Guard-page software COW (88th pass) -- see this module's own doc comment ("87th pass" design,
// "88th pass" implementation) for the full correctness argument. Everything below this point is
// PARENT-side: it runs in whichever process is granting a lazy fork child access to ITS OWN
// memory, is only ever reachable when both `lazy_fork_commit_enabled()` AND `guard_cow_enabled()`
// are true, and changes nothing about any other path when either is unset.
// ============================================================================================

/// One page's worth of guard-cow state, shared (via `ReadProcessMemory`, never real cross-process
/// shared memory) between the PARENT's own [`guard_cow_write_fault_veh`] (writer) and every
/// lazily-faulting CHILD's [`lazy_commit_veh`] (reader). `#[repr(C)]` so the layout this process
/// computes for itself and the layout a child (the SAME compiled binary, `current_exe()`-
/// respawned) computes for the address it read out of [`FORK_CHILD_GUARD_COW_TABLE_ENV_VAR`] are
/// guaranteed identical -- both sides use `core::mem::offset_of!`/`core::mem::size_of::<Self>()`
/// rather than any hand-carried constant.
///
/// `bytes` is `UnsafeCell`-wrapped so [`guard_cow_write_fault_veh`] can mutate it through the
/// shared `&'static [GuardSnapshotSlot]` table reference soundly (not merely "in practice", per
/// Rust's own aliasing rules) -- soundness of doing so without an additional per-slot lock rests
/// on `state`'s own one-shot 0->1 transition, `VIRTUAL_PROTECT_LOCK` serializing every writer that
/// could ever reach the SAME slot (two threads write-faulting the same page), and every access
/// from a DIFFERENT process (a child's `lazy_commit_veh`) going through `ReadProcessMemory`, which
/// never aliases this process's own references at all.
#[repr(C)]
struct GuardSnapshotSlot {
    /// `0` = not yet captured; `1` = captured, published with `Ordering::Release` (see
    /// [`guard_cow_write_fault_veh`]) so a foreign `ReadProcessMemory` of `bytes` that happens
    /// AFTER observing `state == 1` is guaranteed to see the fully-written snapshot, never a torn
    /// write.
    state: AtomicU8,
    bytes: UnsafeCell<[u8; PAGE_SIZE]>,
}

// Safety: see this struct's own doc comment -- every real concurrent access to the SAME slot's
// `bytes` from THIS process is serialized by `VIRTUAL_PROTECT_LOCK`; access from another process
// is always via `ReadProcessMemory`, which does not participate in Rust's aliasing model at all.
unsafe impl Sync for GuardSnapshotSlot {}

impl GuardSnapshotSlot {
    fn zeroed() -> Self {
        GuardSnapshotSlot {
            state: AtomicU8::new(0),
            bytes: UnsafeCell::new([0u8; PAGE_SIZE]),
        }
    }
}

/// A `PROCESS_QUERY_LIMITED_INFORMATION`-only handle to one fork child, shared (via [`Arc`])
/// across every [`PendingGeneration`] this SAME claim ever registers -- see [`GuardCowClaim`]'s
/// own `child_handle` field doc comment for why one handle per CLAIM (not per page) is what
/// [`guard_one_page`] actually needs. Closed exactly once, when the last `Arc` referencing it
/// drops (the last page this claim was pending on either gets serviced/pruned, or the claim
/// itself is torn down).
struct SharedChildHandle(Handle);

// Safety: an opaque Win32 kernel-object handle (`*mut c_void`-shaped, not a real pointer into
// this process's own memory) -- safe to read/close from any thread, exactly as `windows_sys`' own
// raw `HANDLE` type is used everywhere else in this codebase across thread boundaries.
unsafe impl Send for SharedChildHandle {}
unsafe impl Sync for SharedChildHandle {}

impl Drop for SharedChildHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
        // 102nd pass (see `GUARD_COW_OPEN_CLAIMS`'s own doc comment): exactly one
        // `SharedChildHandle` is ever constructed per successful `GuardCowClaim` (the single
        // construction site is `get_or_open_child_handle`, called at most once per claim). Its
        // `Arc` is cloned into the claim's own `child_handle` field and into every
        // `PendingGeneration` the claim registers, so this `Drop` runs exactly once per claim,
        // exactly when the LAST page still relying on this claim's snapshot data has been
        // serviced, pruned-dead, or aborted -- i.e. exactly when this claim's leaked snapshot
        // table can no longer be read by anything, parent or child. That is the real end of this
        // claim's lifetime for admission-control purposes, not the much-shorter-lived
        // `GuardCowClaim` struct itself (see `try_claim_guard_cow_table`).
        GUARD_COW_OPEN_CLAIMS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// One live generation's stake in a single guarded page -- see this module's own "89th pass" doc
/// section for the derivation. `live_handle` is a `PROCESS_QUERY_LIMITED_INFORMATION`-only handle,
/// SHARED (via [`Arc`]) with every other page this SAME claim/generation is pending on -- see
/// [`SharedChildHandle`]'s own doc comment. Checking liveness through a handle an entry keeps a
/// reference to, rather than re-resolving a bare pid, is immune to PID reuse for as long as the
/// entry is outstanding -- a real risk once a page can stay pending for the lifetime of an
/// arbitrarily long-lived generation, not just a single claim/reclaim decision.
///
/// **Bug 6a (found 100th pass, FIXED here)**: this used to be a PER-PAGE `Handle`, opened via a
/// fresh `OpenProcess` call inside [`guard_one_page`] for every single page it guarded -- even
/// though every page a single claim ever guards shares the exact same `child_pid` by construction
/// (one claim == one fork == one child). A multi-MB lazy-eligible group (a guest heap group is the
/// common real-world case) is hundreds to thousands of 4 KiB pages, so a SINGLE guarded fork used
/// to open one handle PER PAGE to the very same child process -- real, measurable kernel
/// handle-table and syscall pressure that grows with GROUP SIZE, not fork count, and stacks on top
/// of every other still-open claim from a long-lived, repeatedly-forking parent (the 99th pass's
/// own "many sequential forks" regression shape). Sharing one handle per claim turns this into
/// O(claims) instead of O(pages) with no change in liveness semantics (a single shared handle is
/// just as immune to PID reuse as one-per-page was, since it is still opened once and kept alive
/// for exactly as long as any page it backs is still pending).
struct PendingGeneration {
    live_handle: std::sync::Arc<SharedChildHandle>,
    slot: &'static GuardSnapshotSlot,
}

// Safety: `slot: &'static GuardSnapshotSlot` is `Sync` already (see that struct's own doc
// comment); `Arc<SharedChildHandle>` is `Send`/`Sync` because `SharedChildHandle` is.
unsafe impl Send for PendingGeneration {}
unsafe impl Sync for PendingGeneration {}

/// `true` iff the generation's own owning process is still running, checked through the entry's
/// OWN kept (shared) handle (see [`PendingGeneration`]'s own doc comment for why this, not a
/// re-resolved pid).
fn pending_generation_alive(p: &PendingGeneration) -> bool {
    let mut exit_code: u32 = 0;
    let ok = unsafe { GetExitCodeProcess(p.live_handle.0, &raw mut exit_code) };
    ok != 0 && exit_code == STILL_ACTIVE
}

fn open_liveness_handle(pid: u32) -> Option<Handle> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() { None } else { Some(handle) }
}

/// One page's shared guard-cow bookkeeping, keyed by page-aligned address in THIS (parent)
/// process's own address space -- see this module's own "89th pass" doc section for the full
/// derivation of why a single entry, shared by however many generations are currently pending,
/// is correct (not merely convenient) in place of one-shadow-per-generation versioning.
struct PageGuardEntry {
    /// This page's real protection from before ANY generation in the CURRENT interval touched it.
    /// Captured once, by whichever generation is first to guard this page after the previous
    /// interval (if any) closed; restored verbatim when this interval closes. Kept (not removed)
    /// once `pending` empties -- see [`guard_cow_write_fault_veh`]'s own doc comment for why an
    /// emptied-but-present entry, not a deleted one, is what makes a genuinely concurrent second
    /// write fault on the same page resolve correctly.
    true_original_protect: u32,
    /// Every live generation currently relying on this page staying stable since it joined.
    /// Pruned lazily (dead entries dropped, closing their handles) whenever a new generation wants
    /// to join this same page -- the direct generalization of the 88th pass's own Bug 5 fix.
    pending: Vec<PendingGeneration>,
}

/// Process-local (see this module's own "89th pass"/87th-pass-finding-1 doc section for why this
/// is correct without a shared-arena structure): every guarded page currently open across every
/// live guard-cow claim THIS process has ever granted as a parent.
static GUARD_PAGE_REGISTRY: Mutex<Option<HashMap<usize, PageGuardEntry>>> = Mutex::new(None);

/// 102nd pass -- count of guard-cow claims currently open (admitted by [`try_claim_guard_cow_table`],
/// not yet fully retired -- see [`SharedChildHandle`]'s `Drop` and [`GuardCowClaim`]'s `Drop`).
///
/// **Why this exists.** Each successful claim `Box::leak`s a [`GuardSnapshotSlot`] table sized ONE
/// FULL PAGE per guarded guest page (`size_of::<GuardSnapshotSlot>() == PAGE_SIZE + 1`, rounded) --
/// i.e. a claim guarding an 8 MiB heap/stack group eagerly commits ~8 MiB in the PARENT process, at
/// claim time, unconditionally, regardless of whether the child ever touches a single one of those
/// pages before `execve`/exit discards the whole mapping. This is a real, already-documented,
/// accepted per-claim cost (this module's own "88th pass"/"98th pass" doc sections) -- but the 98th
/// pass's generalization from "one outstanding claim per parent, ever" (the 88th pass's
/// `GUARD_STATE` single-owner gate) to "unboundedly many concurrent claims, limited only by the
/// UNRELATED `live_cross_process_fork_children` cap (6, system-wide across every parent process)"
/// removed the OLD gate's incidental side effect of also rate-limiting how many of these
/// full-group-sized tables could be simultaneously alive+leaked. Under the old gate, any fork that
/// temporally overlapped an already-open claim fell back to fully EAGER (no table allocated, no
/// leak, only the ordinary child-side copy cost) -- so during a real boot's fork storm, most
/// overlapping forks from a busy parent paid zero extra parent-side commit. Under the new code,
/// EVERY lazy-eligible fork succeeds in claiming lazily and EVERY one leaks its own full-size table,
/// so a busy parent now accumulates up to 6x (the `live_cross_process_fork_children` cap) as many
/// simultaneously-live leaked tables as before, on top of the pre-existing unconditional per-claim
/// leak (never freed, by design, once opened) -- a plausible, code-grounded explanation for the
/// 99th-101st passes' observed crater-speed regression (craters in ~14-21s at 7 processes, vs. the
/// pre-98th-pass code's documented stable 195-300s window at 9-16 processes) that does not require
/// disputing the 101st pass's own real negative result ruling out per-page `VirtualProtect`
/// overhead as the cause.
///
/// **The fix**: restore the old gate's rate-limiting side effect, generalized to N>1 instead of
/// hardcoding N=1 (which would reintroduce the exact TOCTOU the 98th pass fixed for a genuinely
/// concurrent multi-child-from-one-parent workload) -- [`GUARD_COW_CONCURRENT_CLAIM_CAP`] concurrent
/// open claims are allowed per parent process; a fork attempted beyond the cap declines the claim
/// (`try_claim_guard_cow_table` returns `None`) and the caller falls all the way back to eager for
/// every group, exactly as the pre-98th-pass code did for ANY overlap. This bounds worst-case
/// simultaneous parent-side leaked-table commit to `CAP * (largest concurrently-open group size)`
/// instead of `6 * (...)`, while still allowing the 98th pass's own motivating case (a small number
/// of genuinely concurrent children from the same parent, e.g. two XFCE session daemons forking
/// close together) to go lazy correctly rather than falling back to the single-owner gate's
/// pessimistic eager-always-on-overlap behavior.
///
/// **Not yet empirically tuned against a real boot** -- [`GUARD_COW_CONCURRENT_CLAIM_CAP`]'s value
/// is a reasoned starting point (small enough to meaningfully bound the regression this doc comment
/// describes, large enough to still exercise the 98th pass's own multi-generation correctness fix
/// in the isolated concurrent-multi-fork repro), not a value chosen from real boot A/B timing data.
/// A future pass with real boot time should retune it using `LITEBOX_DIAG_FORK_VMA_BREAKDOWN=1`
/// (extended, if needed, to report `GUARD_COW_OPEN_CLAIMS`'s own high-water mark) against
/// `de_only_xcensus_seed3.tar` A/B runs at a few different cap values.
static GUARD_COW_OPEN_CLAIMS: AtomicU32 = AtomicU32::new(0);

/// See [`GUARD_COW_OPEN_CLAIMS`]'s own doc comment for the full derivation of why this cap exists
/// and why its exact value is a reasoned default, not yet an empirically-tuned one.
const GUARD_COW_CONCURRENT_CLAIM_CAP: u32 = 3;
static GUARD_VEH_INSTALLED: OnceLock<()> = OnceLock::new();

fn ensure_guard_cow_veh_installed() {
    GUARD_VEH_INSTALLED.get_or_init(|| {
        unsafe {
            AddVectoredExceptionHandler(1, Some(guard_cow_write_fault_veh));
        }
    });
}

/// Registers exactly one page as guarded on behalf of `claim`: either joins an already-open
/// interval (page already `PAGE_READONLY`, some other still-live generation from this SAME parent
/// forked during the same unbroken no-write span -- see the module doc's own correctness
/// derivation for why sharing is sound here), or, if none is open (no entry, or every previously
/// pending generation has since died), opens a brand new one: `VirtualProtect`s this page to
/// `PAGE_READONLY` and records its real prior protection.
///
/// Holds [`GUARD_PAGE_REGISTRY`]'s lock for this whole decide-then-act body, with
/// [`crate::VIRTUAL_PROTECT_LOCK`] nested inside it for the actual syscall (same lock order
/// [`guard_cow_write_fault_veh`] uses) -- so no other thread on this same parent, concurrently
/// forking and touching the SAME page, can observe or act on a half-decided state between "decide
/// fresh vs join" and "act on that decision".
///
/// A page whose `VirtualProtect` call itself fails is simply left unguarded (not fatal to the
/// group) -- narrows Bug 4's original TOCTOU risk to just that one page, never a NEW hazard beyond
/// what already existed before this mechanism did.
fn guard_one_page(claim: &mut GuardCowClaim, child_pid: u32, page: usize, slot_index: usize, diag: bool) {
    let Some(slot) = claim.table.get(slot_index) else {
        return;
    };
    let mut registry = GUARD_PAGE_REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let map = registry.get_or_insert_with(HashMap::new);

    // Prune dead pending generations from any existing entry first -- the direct generalization
    // of the 88th pass's own Bug 5 fix (there: per-claim, on a single-slot reclaim; here: per
    // page, on every join attempt).
    if let Some(entry) = map.get_mut(&page) {
        let before = entry.pending.len();
        entry.pending.retain(pending_generation_alive);
        if diag && before > 0 && entry.pending.len() < before {
            eprintln!(
                "[lazy_fork_commit] guard-cow DIAG: page={page:#x} pruned {} dead pending (had {before}, now {})",
                before - entry.pending.len(),
                entry.pending.len()
            );
        }
    }

    // Heal-then-remove ANY entry now found with an EMPTY pending list, whatever the cause --
    // pruning just above just emptied it, OR it was born empty already (the "child died between
    // spawn and guard" branch below inserts exactly such an entry). Both are a CLOSED interval:
    // the real Windows page protection is still whatever the LAST successful `VirtualProtect`
    // call in this function set it to (`PAGE_READONLY`), which is NOT this entry's own recorded
    // `true_original_protect` -- nobody has restored it, because restoration only ever happens
    // via a genuine parent WRITE fault, which by definition never came (that is exactly what
    // "zero live pending generations" means: nobody is left who could ever cause one to matter,
    // but the page itself was never told). Leaving a closed-but-unhealed entry in place is the
    // generalized form of the 88th pass's own Bug 5: the FRESH-GUARD branch below would then
    // `VirtualProtect` an ALREADY-`PAGE_READONLY` page and capture the CURRENT (poisoned,
    // still-`PAGE_READONLY`) value as if it were the true original -- confirmed live this pass
    // (`LITEBOX_DIAG_LAZY_FORK_COMMIT=1`, a 3-concurrent-subshell repro with real interleaved
    // parent heap writes): `true_original_protect=0x2` (`PAGE_READONLY`) recorded for a page,
    // followed by an unbounded same-instruction re-fault loop (`guard_cow_write_fault_veh`
    // "restoring" to the same already-`PAGE_READONLY` value forever) burning 100+ CPU-seconds
    // with zero forward progress -- a real, reproducible hang, not a theoretical concern. Healing
    // HERE, eagerly, the moment an entry is found closed (rather than only reactively inside the
    // write-fault handler, which by definition never fires again for a page nobody ever writes to
    // again) is what closes this for good.
    if let Some(entry) = map.get(&page)
        && entry.pending.is_empty()
    {
        let true_original_protect = entry.true_original_protect;
        map.remove(&page);
        let _vp_guard = crate::VIRTUAL_PROTECT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut discard_old: u32 = 0;
        unsafe {
            VirtualProtect(page as *mut c_void, PAGE_SIZE, true_original_protect, &mut discard_old);
        }
        if diag {
            eprintln!(
                "[lazy_fork_commit] guard-cow: page={page:#x} healed to true_original_protect={true_original_protect:#x} (closed interval, no live pending generations)"
            );
        }
    }

    if map.get(&page).is_some_and(|e| !e.pending.is_empty()) {
        let Some(handle) = get_or_open_child_handle(claim, child_pid) else {
            return;
        };
        let entry = map.get_mut(&page).expect("checked non-empty just above");
        entry.pending.push(PendingGeneration {
            live_handle: handle,
            slot,
        });
        claim.joined_pages.push((page, slot));
        if diag {
            eprintln!(
                "[lazy_fork_commit] guard-cow: page={page:#x} joined open interval ({} pending)",
                entry.pending.len()
            );
        }
        return;
    }

    // Not currently open (no entry, or pruned to empty) -- open a fresh interval.
    let mut old_protect: u32 = 0;
    let protected = {
        let _vp_guard = crate::VIRTUAL_PROTECT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { VirtualProtect(page as *mut c_void, PAGE_SIZE, PAGE_READONLY, &mut old_protect) != 0 }
    };
    if !protected {
        map.remove(&page);
        return;
    }
    match get_or_open_child_handle(claim, child_pid) {
        Some(handle) => {
            map.insert(
                page,
                PageGuardEntry {
                    true_original_protect: old_protect,
                    pending: vec![PendingGeneration {
                        live_handle: handle,
                        slot,
                    }],
                },
            );
            claim.joined_pages.push((page, slot));
            if diag {
                eprintln!(
                    "[lazy_fork_commit] guard-cow: page={page:#x} opened fresh interval old_protect={old_protect:#x}"
                );
            }
        }
        None => {
            // The child died in the instant between spawn and here -- still record the entry
            // (empty pending) so this page's TRUE prior protection is not lost: a real future
            // write-fault must still find it and restore it, rather than leaving it stuck
            // PAGE_READONLY forever with nothing tracking what to heal it back to.
            map.insert(
                page,
                PageGuardEntry {
                    true_original_protect: old_protect,
                    pending: Vec::new(),
                },
            );
        }
    }
}

/// Batched fast path for [`reserve_group_lazy_guarded`]'s inner loop (101st pass): when an ENTIRE
/// contiguous, already-`VirtualQuery`-uniform sub-region (so every page in it shares the same real
/// prior protection by construction) contains NO existing [`GUARD_PAGE_REGISTRY`] entry at all --
/// the common, non-overlapping case, since two live generations from the SAME parent guarding the
/// exact same address range concurrently is bounded and rare (`live_cross_process_fork_children`'s
/// admission cap, `AGENTS.md`'s 76th-pass finding) -- guards every page in `region_start..
/// region_end` with ONE `VirtualProtect` call and ONE [`GUARD_PAGE_REGISTRY`] lock acquisition,
/// instead of [`guard_one_page`]'s existing O(pages) `VirtualProtect`+lock-acquire-per-page cost
/// (flagged as "a known, explicit, un-optimized cost" by the 89th pass's own doc comment above,
/// the concrete follow-up this implements). Returns `false` the instant ANY page in the region
/// already has an entry (or the batched `VirtualProtect` itself fails) -- the caller then falls
/// back to the existing, unmodified, per-page [`guard_one_page`] loop for that WHOLE sub-region,
/// so the join-vs-open decision's own correctness for a genuine overlap is completely unaffected;
/// this function never partially mutates registry state on a path that returns `false` (the
/// pre-check and the `VirtualProtect` call are both all-or-nothing for the region).
///
/// Holds [`GUARD_PAGE_REGISTRY`]'s lock for the ENTIRE decide-then-act span, exactly like
/// [`guard_one_page`] -- so no other thread on this same parent can observe or act on a
/// half-decided state for any page in the region between the pre-check and the batched
/// `VirtualProtect`/insert (same race this module's own "89th pass" doc section already closed
/// for the per-page path).
fn try_guard_region_batched(
    claim: &mut GuardCowClaim,
    child_pid: u32,
    region_start: usize,
    region_end: usize,
    source_group: &Range<usize>,
    group_slot_base: usize,
    diag: bool,
) -> bool {
    let mut registry = GUARD_PAGE_REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let map = registry.get_or_insert_with(HashMap::new);

    let mut probe = region_start;
    while probe < region_end {
        if map.contains_key(&probe) {
            return false;
        }
        probe += PAGE_SIZE;
    }

    let mut old_protect: u32 = 0;
    let protected = {
        let _vp_guard = crate::VIRTUAL_PROTECT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe {
            VirtualProtect(
                region_start as *mut c_void,
                region_end - region_start,
                PAGE_READONLY,
                &mut old_protect,
            ) != 0
        }
    };
    if !protected {
        return false;
    }

    let Some(handle) = get_or_open_child_handle(claim, child_pid) else {
        // Child died in the instant between spawn and here -- same handling as guard_one_page's
        // own "None" branch: still record every page's true prior protection (empty pending) so a
        // future real write-fault still finds it and restores it, rather than it staying stuck
        // PAGE_READONLY forever with nothing tracking what to heal it back to.
        let mut page = region_start;
        while page < region_end {
            map.insert(
                page,
                PageGuardEntry {
                    true_original_protect: old_protect,
                    pending: Vec::new(),
                },
            );
            page += PAGE_SIZE;
        }
        return true;
    };

    let mut page = region_start;
    while page < region_end {
        let slot_index = group_slot_base + (page - source_group.start) / PAGE_SIZE;
        let Some(slot) = claim.table.get(slot_index) else {
            page += PAGE_SIZE;
            continue;
        };
        map.insert(
            page,
            PageGuardEntry {
                true_original_protect: old_protect,
                pending: vec![PendingGeneration {
                    live_handle: std::sync::Arc::clone(&handle),
                    slot,
                }],
            },
        );
        claim.joined_pages.push((page, slot));
        page += PAGE_SIZE;
    }
    if diag {
        eprintln!(
            "[lazy_fork_commit] guard-cow: batched region {region_start:#x}..{region_end:#x} ({} pages) opened fresh in one VirtualProtect call, old_protect={old_protect:#x}",
            (region_end - region_start) / PAGE_SIZE
        );
    }
    true
}

/// Parent-side setup, called once per lazy-eligible group when guard-cow is enabled for this fork:
/// reserves the group in the child exactly as [`reserve_group_lazy`] does, THEN walks this
/// PARENT's own already-committed pages across `source_group` `VirtualQuery`-region by region. For
/// each committed, non-guard/no-access sub-range, tries [`try_guard_region_batched`] first (101st
/// pass -- one `VirtualProtect`/lock-acquire for the whole sub-range when it is entirely unopened),
/// falling back to the original per-page [`guard_one_page`] loop only when that returns `false`
/// (some page in the sub-range already has a live registry entry, or the batched call itself
/// failed) -- see [`try_guard_region_batched`]'s own doc comment for why this is correctness-
/// preserving relative to the pure per-page path it augments, not replaces.
#[must_use]
fn reserve_group_lazy_guarded(
    claim: &mut GuardCowClaim,
    child_pid: u32,
    child: Handle,
    source_group: &Range<usize>,
    group_slot_base: usize,
) -> GroupCopyResult {
    let result = reserve_group_lazy(child, source_group);
    if !result.succeeded {
        return result;
    }
    let diag = std::env::var_os("LITEBOX_DIAG_LAZY_FORK_COMMIT").is_some();
    let mut addr = source_group.start;
    while addr < source_group.end {
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        let ok = unsafe {
            VirtualQuery(
                addr as *const c_void,
                &raw mut mbi,
                core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            ) != 0
        };
        if !ok || mbi.RegionSize == 0 {
            // Can't make forward progress querying this parent's own address space -- stop
            // guarding further sub-ranges of this group rather than looping forever; whatever
            // wasn't guarded simply keeps the pre-existing live-read behavior.
            break;
        }
        let region_start = addr.max(mbi.BaseAddress as usize);
        let region_end = ((mbi.BaseAddress as usize).saturating_add(mbi.RegionSize)).min(source_group.end);
        if mbi.State == MEM_COMMIT && mbi.Protect & (PAGE_NOACCESS | PAGE_GUARD) == 0 && region_end > region_start {
            let batched = try_guard_region_batched(
                claim,
                child_pid,
                region_start,
                region_end,
                source_group,
                group_slot_base,
                diag,
            );
            if !batched {
                let mut page = region_start;
                while page < region_end {
                    let slot_index = group_slot_base + (page - source_group.start) / PAGE_SIZE;
                    guard_one_page(claim, child_pid, page, slot_index, diag);
                    page += PAGE_SIZE;
                }
            }
        }
        addr = region_end.max(addr + 1);
    }
    result
}

/// Public handle for an in-progress guard-cow claim, spanning from
/// [`try_claim_guard_cow_table`] (pre-spawn, so the table's base address can be put into the
/// child's environment block) through either [`finalize_guard_cow_table`] (fork succeeded, at
/// least attempted) or [`abort_guard_cow_claim`] (fork's own spawn failed, or the per-group copy
/// loop failed partway through) -- see `spawn_process_fork_child`'s own call sites for exactly
/// where each of the three functions belongs.
pub struct GuardCowClaim {
    table: &'static [GuardSnapshotSlot],
    /// (page address, this claim's own slot for that page) pairs this claim has successfully
    /// registered itself as a pending generation for -- lets [`abort_guard_cow_claim`] undo
    /// PRECISELY what this claim did and nothing belonging to any other, independently-pending
    /// generation this claim happened to share a page's open interval with.
    joined_pages: Vec<(usize, &'static GuardSnapshotSlot)>,
    /// This claim's own single, shared liveness handle to its one child pid -- see
    /// [`SharedChildHandle`]'s own doc comment (Bug 6a, 100th pass). Opened lazily, on the first
    /// page this claim ever successfully guards/joins (via [`get_or_open_child_handle`]), and
    /// cloned (cheap `Arc` refcount bump, no new `OpenProcess` syscall) into every subsequent
    /// [`PendingGeneration`] this same claim registers, however many thousands of pages that ends
    /// up being.
    child_handle: Option<std::sync::Arc<SharedChildHandle>>,
}

/// 102nd pass: releases this claim's [`GUARD_COW_OPEN_CLAIMS`] admission slot for the ONE real edge
/// case [`SharedChildHandle`]'s own `Drop` can never cover -- a claim that was admitted (successful
/// `try_claim_guard_cow_table`) but ended up never guarding a single page (e.g. the fork's own
/// per-group reservation failed before `guard_one_page`/`get_or_open_child_handle` was ever called
/// for any group), so `child_handle` stays `None` for this claim's entire lifetime and no
/// `SharedChildHandle` is ever constructed to decrement on. When `child_handle` IS `Some`, this is
/// deliberately a no-op: responsibility for the decrement belongs solely to `SharedChildHandle`'s
/// own `Drop`, which fires once, whenever the LAST `Arc` reference (this claim's own field, plus one
/// clone per `PendingGeneration` the claim registered) drops -- not when this much shorter-lived
/// `GuardCowClaim` struct itself goes out of scope (`finalize_guard_cow_table` drops it immediately
/// after installing the VEH, while the pages it guarded keep being serviced far longer).
impl Drop for GuardCowClaim {
    fn drop(&mut self) {
        if self.child_handle.is_none() {
            GUARD_COW_OPEN_CLAIMS.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Returns this claim's own shared liveness handle (see [`SharedChildHandle`]/[`GuardCowClaim::
/// child_handle`]'s doc comments), opening it via a single `OpenProcess` call the first time this
/// claim needs one and reusing that same handle (cheap `Arc` clone) for every later page -- Bug 6a
/// (100th pass): this used to be a fresh `OpenProcess` per PAGE, which for a large lazy-eligible
/// group (a guest heap group routinely spans hundreds-to-thousands of pages) meant one claim could
/// open thousands of redundant handles to the very same child process.
fn get_or_open_child_handle(
    claim: &mut GuardCowClaim,
    child_pid: u32,
) -> Option<std::sync::Arc<SharedChildHandle>> {
    if let Some(h) = &claim.child_handle {
        return Some(std::sync::Arc::clone(h));
    }
    let handle = open_liveness_handle(child_pid)?;
    let arc = std::sync::Arc::new(SharedChildHandle(handle));
    claim.child_handle = Some(std::sync::Arc::clone(&arc));
    Some(arc)
}

/// Parent-side, PRE-spawn: allocates (and `'static`-leaks) a snapshot table sized for
/// `total_pages` guest pages -- the sum, across every group this fork intends to guard, of
/// `group.len().div_ceil(PAGE_SIZE)`. Unlike the 88th pass's single-outstanding-child version,
/// this always succeeds for any real caller (`total_pages > 0` always holds at the real call
/// site) -- kept as `Option` for interface stability and as a defensive hook for a future
/// resource cap, not because there is a live contention path today: per-fork admission is already
/// bounded upstream by `litebox_shim_linux::GlobalState`'s own `live_cross_process_fork_children`
/// cap (see this module's own "89th pass" doc section).
#[must_use]
pub fn try_claim_guard_cow_table(total_pages: usize) -> Option<GuardCowClaim> {
    if total_pages == 0 {
        return None;
    }
    // 102nd pass: admission control on CONCURRENT open claims -- see `GUARD_COW_OPEN_CLAIMS`'s own
    // doc comment. A CAS loop (not a bare `fetch_add` then check-and-revert) so a decline never
    // transiently bumps the counter above the cap for another thread's own concurrent check to see.
    let mut current = GUARD_COW_OPEN_CLAIMS.load(Ordering::Acquire);
    loop {
        if current >= GUARD_COW_CONCURRENT_CLAIM_CAP {
            if std::env::var_os("LITEBOX_DIAG_LAZY_FORK_COMMIT").is_some() {
                eprintln!(
                    "[lazy_fork_commit] guard-cow claim DECLINED (cap): {current} open claims >= cap {GUARD_COW_CONCURRENT_CLAIM_CAP}, falling this fork back to eager"
                );
            }
            return None;
        }
        match GUARD_COW_OPEN_CLAIMS.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
    let table: Vec<GuardSnapshotSlot> = (0..total_pages).map(|_| GuardSnapshotSlot::zeroed()).collect();
    let table: &'static [GuardSnapshotSlot] = Box::leak(table.into_boxed_slice());
    Some(GuardCowClaim {
        table,
        joined_pages: Vec::new(),
        child_handle: None,
    })
}

/// The table's base address, to serialize into [`FORK_CHILD_GUARD_COW_TABLE_ENV_VAR`].
#[must_use]
pub fn guard_cow_claim_table_base(claim: &GuardCowClaim) -> usize {
    claim.table.as_ptr() as usize
}

/// Parent-side, POST-spawn (needs the real child `Handle`/pid): reserves+guards one lazy-eligible
/// group under an already-successful [`GuardCowClaim`] -- see [`reserve_group_lazy_guarded`].
#[must_use]
pub fn guard_cow_reserve_group(
    claim: &mut GuardCowClaim,
    child_pid: u32,
    child: Handle,
    source_group: &Range<usize>,
    group_slot_base: usize,
) -> GroupCopyResult {
    reserve_group_lazy_guarded(claim, child_pid, child, source_group, group_slot_base)
}

/// Commits a successful claim. Every page this claim guarded was already registered live, under
/// [`GUARD_PAGE_REGISTRY`]'s own lock, at the exact moment [`guard_one_page`] protected or joined
/// it (unlike the 88th pass's single `GUARD_STATE`, which was only populated HERE, at finalize
/// time -- closing a narrow pre-existing race where a page already made `PAGE_READONLY` mid-loop,
/// but not yet described to `guard_cow_write_fault_veh`, could be written by a DIFFERENT guest
/// thread of the same multi-threaded parent before finalize ever ran). This function therefore
/// installs the VEH (idempotent past the first call, harmless if it was never needed) and lets
/// `claim` drop -- which intentionally does NOT release anything: the whole point is for these
/// pages to keep being serviced for as long as their pending generations stay alive.
pub fn finalize_guard_cow_table(claim: GuardCowClaim, _child_pid: u32) {
    ensure_guard_cow_veh_installed();
    drop(claim);
}

/// Abandons a claim that never became a real, running guarded fork (the child's own spawn or
/// per-group copy loop failed). Removes precisely this claim's OWN pending entry from every page
/// it joined (dropping that entry's `Arc<SharedChildHandle>` reference, which closes the shared
/// liveness handle once every page this claim registered it for has released it), and, for any
/// page this claim was the LAST live generation on, restores that page to its real prior
/// protection -- without this, a partially-guarded set of pages this claim was the sole owner of
/// would stay stuck `PAGE_READONLY` forever.
pub fn abort_guard_cow_claim(claim: GuardCowClaim) {
    let mut registry = GUARD_PAGE_REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(map) = registry.as_mut() else {
        return;
    };
    for (page_addr, slot) in &claim.joined_pages {
        let Some(entry) = map.get_mut(page_addr) else {
            continue;
        };
        entry.pending.retain(|p| !core::ptr::eq(p.slot, *slot));
        if entry.pending.is_empty() {
            let _vp_guard = crate::VIRTUAL_PROTECT_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut discard_old: u32 = 0;
            unsafe {
                VirtualProtect(
                    *page_addr as *mut c_void,
                    PAGE_SIZE,
                    entry.true_original_protect,
                    &mut discard_old,
                );
            }
        }
    }
}

/// **Bug 6b (found 100th pass, FIXED here)**: [`GUARD_PAGE_REGISTRY`] previously had no hook into
/// this process's own `PageManagementProvider` impl (`WindowsUserland::update_permissions`/
/// `deallocate_pages`, `lib.rs`) -- so an entirely ordinary, unrelated GUEST syscall
/// (`mprotect()`/`munmap()`) landing on a page this process currently guard-cow-protects on behalf
/// of one or more pending fork children could silently desync the registry from the REAL Windows
/// page state, in either direction:
///
/// - `update_permissions` calls `VirtualProtect` directly and DISCARDS the `old_protect` it gets
///   back (only ever used for a diagnostic log) -- so a guest `mprotect()` that happens to target a
///   currently-guarded (`PAGE_READONLY`) page can silently flip the REAL protection to whatever the
///   guest asked for (commonly back to read-write) with **no fault at all**, since `VirtualProtect`
///   does not itself fault. [`guard_cow_write_fault_veh`] only ever runs from an
///   `EXCEPTION_ACCESS_VIOLATION` -- an ordinary `VirtualProtect` call is not one, so this
///   mechanism's own write-fault capture is silently bypassed entirely. Any pending generation
///   still relying on that page's staying stable (the entire point of guard-cow) now gets nothing:
///   the guest is free to write to it before ever taking a captured snapshot, and a lazily-faulting
///   child that races that write (the double-checked-state protocol only guards against the
///   PARENT's own write-FAULT path, not an mprotect-then-plain-write sequence that never faults)
///   can observe a torn value -- reopening the exact TOCTOU class (Bug B/Bug 4) this whole
///   mechanism exists to close, this time via a perfectly ordinary guest syscall rather than a
///   same-page write race between two processes.
/// - Symmetrically, `deallocate_pages` can `VirtualFree(MEM_DECOMMIT)` a currently-guarded page
///   out from under the registry -- a later, unrelated re-mmap of the SAME host virtual address
///   (routine on Windows, which readily reuses a freed VA range) could then have its own, entirely
///   different real protection silently STOMPED by a stale [`guard_one_page`]/[`abort_guard_cow_
///   claim`] "heal" (`VirtualProtect(..., true_original_protect, ...)`) applied to what the
///   registry still thinks is the SAME allocation it originally guarded, potentially corrupting or
///   mis-protecting completely unrelated, later memory.
///
/// This is exactly the kind of "accumulated stale state across many sequential forks" class the
/// 99th pass's regression pointed at: the more pages a long-lived, repeatedly-forking parent has
/// EVER guarded over its lifetime, the more of its own later, entirely ordinary heap/mmap activity
/// has a chance to land on one of them.
///
/// **Fix**: [`WindowsUserland::update_permissions`]/`deallocate_pages` (`lib.rs`) now call this
/// function on `range` BEFORE performing their own real `VirtualProtect`/`VirtualFree` call. For
/// every currently-guarded page inside `range`, this immediately services every still-alive
/// pending generation with the page's CURRENT bytes (the same capture step
/// [`guard_cow_write_fault_veh`] itself performs on a genuine write fault) and evicts the page from
/// the registry entirely -- the caller's own real, guest-intended operation then proceeds
/// unimpeded, and no stale entry is left behind to mis-heal a future, unrelated re-use of the same
/// address. A cheap no-op (one `guard_cow_enabled()` check, no lock taken) when guard-cow was never
/// enabled or this process has never guarded anything.
pub fn invalidate_guarded_range(range: &Range<usize>) {
    if !guard_cow_enabled() {
        return;
    }
    let mut registry = GUARD_PAGE_REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(map) = registry.as_mut() else {
        return;
    };
    if map.is_empty() || range.start >= range.end {
        return;
    }
    let diag = std::env::var_os("LITEBOX_DIAG_LAZY_FORK_COMMIT").is_some();
    let mut page = range.start & !(PAGE_SIZE - 1);
    while page < range.end {
        if let Some(entry) = map.remove(&page) {
            let pending_count = entry.pending.len();
            if pending_count > 0 {
                // Best-effort snapshot of whatever is currently there -- read BEFORE the caller's
                // own real VirtualProtect/VirtualFree call runs (this function is always called
                // first), so this is still a coherent "value as of right before the guest's own
                // mprotect/munmap", exactly matching what an eager copy taken at this same instant
                // would have captured.
                let mut buf = [0u8; PAGE_SIZE];
                unsafe {
                    core::ptr::copy_nonoverlapping(page as *const u8, buf.as_mut_ptr(), PAGE_SIZE);
                }
                for p in entry.pending {
                    if pending_generation_alive(&p) {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                buf.as_ptr(),
                                (*p.slot.bytes.get()).as_mut_ptr(),
                                PAGE_SIZE,
                            );
                        }
                        p.slot.state.store(1, Ordering::Release);
                    }
                }
            }
            if diag {
                eprintln!(
                    "[lazy_fork_commit] guard-cow: page={page:#x} INVALIDATED by guest-initiated \
                     protection/lifetime change (evicted from registry, {pending_count} pending \
                     generation(s) serviced) -- NOT restoring true_original_protect, the caller's \
                     own real operation decides this page's next protection"
                );
            }
        }
        page += PAGE_SIZE;
    }
}

/// PARENT-side VEH: catches this process's OWN next write to a page it has guard-protected on
/// behalf of one or more lazy fork children, captures a pre-write snapshot into EVERY still-alive
/// pending generation's own slot (see this module's own "89th pass" doc section for why one
/// capture correctly serves all of them at once), restores the page's real prior protection, and
/// lets the write retry and succeed. Declines (`EXCEPTION_CONTINUE_SEARCH`) every fault that is
/// not a write to a currently-tracked address, including every fault this process's own
/// pre-existing `fork_verify`/main VEH machinery is responsible for -- unchanged by this handler's
/// mere presence.
unsafe extern "system" fn guard_cow_write_fault_veh(info: *mut EXCEPTION_POINTERS) -> i32 {
    let rec = unsafe { &*(*info).ExceptionRecord };
    const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC000_0005;
    if rec.ExceptionCode.cast_unsigned() != EXCEPTION_ACCESS_VIOLATION {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    // ExceptionInformation[0]: 0 = read access that failed, 1 = write access that failed, 8 =
    // DEP/execute. Only a write can legitimately be this mechanism's own guard fault -- a READ
    // fault on a page THIS mechanism protected can never happen (PAGE_READONLY still permits
    // reads), so a read fault here belongs to someone else's fault entirely.
    if rec.ExceptionInformation.first().copied().unwrap_or(0) != 1 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let fault_addr = rec.ExceptionInformation[1];
    let page_addr = fault_addr & !(PAGE_SIZE - 1);

    let mut registry = GUARD_PAGE_REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(map) = registry.as_mut() else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let Some(entry) = map.get_mut(&page_addr) else {
        return EXCEPTION_CONTINUE_SEARCH;
    };

    let diag = std::env::var_os("LITEBOX_DIAG_LAZY_FORK_COMMIT").is_some();
    if diag {
        static LAST_FAULT_PAGE: AtomicUsize = AtomicUsize::new(0);
        static LAST_FAULT_REPEAT: AtomicUsize = AtomicUsize::new(0);
        let prev = LAST_FAULT_PAGE.swap(page_addr, Ordering::Relaxed);
        let repeat = if prev == page_addr {
            LAST_FAULT_REPEAT.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            LAST_FAULT_REPEAT.store(0, Ordering::Relaxed);
            0
        };
        eprintln!(
            "[lazy_fork_commit] guard-cow DIAG: write-fault entry page={page_addr:#x} \
             true_original_protect={:#x} pending_before={} same_page_repeat={repeat}",
            entry.true_original_protect,
            entry.pending.len()
        );
        if repeat > 3 {
            eprintln!(
                "[lazy_fork_commit] guard-cow DIAG: *** SAME-PAGE RE-FAULT LOOP SUSPECTED *** \
                 page={page_addr:#x} true_original_protect={:#x} (0x2=READONLY would mean a \
                 POISONED restore target)",
                entry.true_original_protect
            );
        }
    }

    // Hold VIRTUAL_PROTECT_LOCK for the whole capture-distribute-restore span -- the exact
    // precedent `fork_verify::write_usize_fault_tolerant` already established for taking this
    // lock from inside VEH dispatch (see this module's own doc comment, "87th pass" section,
    // finding 3), nested inside GUARD_PAGE_REGISTRY's own lock (same order [`guard_one_page`]
    // uses, so the two paths can never deadlock against each other).
    let _vp_guard = crate::VIRTUAL_PROTECT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // Leaving `entry` present but with an emptied `pending` (never removing it outright) is what
    // makes a genuinely concurrent SECOND write fault on this SAME page -- another thread, which
    // faulted before either one's restore took effect, now blocked on the lock above until this
    // one finishes -- resolve correctly: it will find `entry` still here with nothing left to
    // capture, skip straight to the idempotent restore-and-continue below, and get its own valid
    // `EXCEPTION_CONTINUE_EXECUTION` too. Deleting the entry instead would leave that second
    // thread's fault matching nothing, with no other handler able to resolve it.
    if !entry.pending.is_empty() {
        let mut buf = [0u8; PAGE_SIZE];
        unsafe {
            core::ptr::copy_nonoverlapping(page_addr as *const u8, buf.as_mut_ptr(), PAGE_SIZE);
        }
        let n = entry.pending.len();
        for p in entry.pending.drain(..) {
            if pending_generation_alive(&p) {
                unsafe {
                    core::ptr::copy_nonoverlapping(buf.as_ptr(), (*p.slot.bytes.get()).as_mut_ptr(), PAGE_SIZE);
                }
                p.slot.state.store(1, Ordering::Release);
            }
        }
        if diag {
            eprintln!(
                "[lazy_fork_commit] guard-cow: parent write-fault captured page={page_addr:#x} generations={n}"
            );
        }
    }

    let mut discard_old: u32 = 0;
    let restored = unsafe {
        VirtualProtect(
            page_addr as *mut c_void,
            PAGE_SIZE,
            entry.true_original_protect,
            &mut discard_old,
        ) != 0
    };
    if !restored {
        // Could not restore write access to the parent's own page -- decline rather than spin or
        // pretend success; the guest takes a real, honest SIGSEGV via the normal unhandled-AV
        // path, which is the correct outcome when this cannot be serviced.
        return EXCEPTION_CONTINUE_SEARCH;
    }
    EXCEPTION_CONTINUE_EXECUTION
}
