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

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicIsize, AtomicU8, AtomicU32, AtomicUsize, Ordering};
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

/// One `VirtualQuery`-uniform, already-committed sub-range of a guarded group, guard-page-
/// protected to `PAGE_READONLY` by [`reserve_group_lazy_guarded`]. `old_protect` is THIS
/// sub-range's own real prior protection (never assumed to be blanket `PAGE_READWRITE` -- a
/// guest-`mprotect`'d read-only sub-range must heal back to read-only, not become newly
/// writable), and `slot_base_index` is this sub-range's own first page's index into the fork's
/// shared [`GuardSnapshotSlot`] table (computed relative to the OWNING GROUP's start, matching
/// the child's own [`GUARD_GROUP_BASES`] computation exactly).
struct GuardedRegion {
    range: Range<usize>,
    old_protect: u32,
    slot_base_index: usize,
}

/// One parent's complete guard-cow bookkeeping for its single currently-outstanding guarded fork
/// child. Only ever `Some` while [`GUARD_COW_OWNER_PID`] is non-zero for a genuinely-guarded fork
/// (a fork that requested guard-cow but was denied the claim never populates this at all -- it
/// falls all the way back to eager, see `spawn_process_fork_child`'s own caller-side logic).
struct GuardCowState {
    regions: Vec<GuardedRegion>,
    /// `'static`-leaked (`Box::leak`) so its address stays valid for exactly as long as this
    /// mechanism could still need to service a fault against it -- intentionally never freed;
    /// see this file's own module doc comment on why release/cleanup for a successfully-`execve`'d
    /// long-lived child is a known, honest, bounded (one slot, not a leak of every fork) limit.
    table: &'static [GuardSnapshotSlot],
}

/// `0` = free. Otherwise the pid of the sole cross-process-fork child THIS process (acting as a
/// parent) currently has an outstanding guard-cow relationship with, OR the reserved sentinel
/// [`GUARD_COW_CLAIM_PLACEHOLDER`] while a claim is being finalized. Process-local by design (87th
/// pass design refinement 1: the correctness unit is per-parent-process, not tree-wide) -- an
/// ordinary static, no shared arena, no cross-process attach needed for the claim/release decision
/// itself.
static GUARD_COW_OWNER_PID: AtomicU32 = AtomicU32::new(0);
/// Not a real pid (`pid_t`/Windows PIDs never reach `u32::MAX`) -- a placeholder occupying the
/// slot between a successful CAS-from-0 and [`finalize_guard_cow_claim`]/[`guard_cow_release_claim`]
/// so two guest threads racing concurrent `fork()`s on the SAME parent can never both believe they
/// hold the slot.
const GUARD_COW_CLAIM_PLACEHOLDER: u32 = u32::MAX;

static GUARD_STATE: Mutex<Option<GuardCowState>> = Mutex::new(None);
static GUARD_VEH_INSTALLED: OnceLock<()> = OnceLock::new();

/// Bounded, non-blocking liveness check: `true` iff `pid` is still a running process. Same
/// `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `GetExitCodeProcess` idiom
/// `WindowsUserland::is_process_alive` (`lib.rs`) already uses for its own dead-holder recovery --
/// deliberately the identical two Win32 calls and verdict rule (not factored into a shared helper
/// this pass, for the same reason that function's own doc comment gives: a different type in a
/// different file, a mechanical dedup for a future pass, not a behavior change now). An unopenable
/// pid is treated as NOT alive (reclaim the slot) -- see [`try_claim_guard_cow_for_fork`]'s own
/// doc comment for why that is the conservative, safe direction for this specific gate (a false
/// "reclaim" only costs this NEXT fork guard-cow eligibility in the rare case the check was
/// wrong, which is a performance loss, not a correctness one -- the fallback is always the
/// already-proven-safe eager path).
fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut exit_code: u32 = 0;
    let ok = unsafe { GetExitCodeProcess(handle, &raw mut exit_code) };
    unsafe { CloseHandle(handle) };
    ok != 0 && exit_code == STILL_ACTIVE
}

/// Parent-side: attempts to claim this process's single guard-cow slot for a NEW fork about to
/// happen. Returns `true` (slot claimed, as [`GUARD_COW_CLAIM_PLACEHOLDER`]) iff either the slot
/// was already free, or its recorded owner pid has since died (reclaimed). Returns `false`
/// (decline -- caller must fall all the way back to eager for every group of this fork, never
/// plain unguarded lazy) whenever another live child from this SAME parent still holds the slot,
/// OR another thread is concurrently mid-claim (observed as the placeholder itself) -- declining
/// rather than spinning/retrying, since the eager fallback is always correct and a missed guard-cow
/// opportunity is only a performance cost.
///
/// Must be paired with EXACTLY ONE of [`finalize_guard_cow_claim`] (the fork proceeded and at
/// least one group was actually guarded) or [`guard_cow_release_claim`] (the fork's own spawn
/// failed, or ended up not needing guard-cow after all) -- never both, never neither.
#[must_use]
fn try_claim_guard_cow_for_fork() -> bool {
    if GUARD_COW_OWNER_PID
        .compare_exchange(
            0,
            GUARD_COW_CLAIM_PLACEHOLDER,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_ok()
    {
        return true;
    }
    let current = GUARD_COW_OWNER_PID.load(Ordering::SeqCst);
    if current == 0 || current == GUARD_COW_CLAIM_PLACEHOLDER {
        return false;
    }
    if !pid_is_alive(current) {
        if GUARD_COW_OWNER_PID
            .compare_exchange(
                current,
                GUARD_COW_CLAIM_PLACEHOLDER,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            // Real bug found+fixed live, 88th pass: heal the PREVIOUS (now-dead-owner) claim's
            // guarded regions back to their own real prior protection BEFORE this new claim can
            // ever re-guard the same address range. Without this, a group whose address range is
            // reused across consecutive forks from the same long-lived parent (the common case --
            // e.g. bash's own heap/stack layout barely moves between consecutive external-command
            // forks) could still be sitting `PAGE_READONLY` from the dead claim's own
            // never-triggered guard (the parent simply never happened to write there before that
            // child died) when `reserve_group_lazy_guarded` runs for the NEW claim.
            // `VirtualProtect`'s own `old_protect` out-param then reports the CURRENT
            // (already-`PAGE_READONLY`) state, not the true pre-guarding one -- poisoning the new
            // claim's own restore target. The next time the PARENT writes to that page,
            // [`guard_cow_write_fault_veh`] "restores" it to that poisoned, still-read-only value,
            // so the very same faulting instruction re-faults the instant it retries --
            // `EXCEPTION_CONTINUE_EXECUTION` loops forever on one instruction, real CPU burned on
            // every iteration's exception dispatch, zero forward progress, no crash. Confirmed
            // live: a real `de_only_xcensus_seed3.tar` boot reproduced exactly this signature (two
            // clean guarded forks, then the parent's own continued execution spins at 100% CPU
            // with zero further progress) -- reconfirmed absent once this fix healed the
            // superseded claim's regions here, before the new claim's own protect walk ever runs.
            heal_superseded_guard_state();
            return true;
        }
        return false;
    }
    false
}

/// Restores every region the CURRENT (about-to-be-superseded) [`GuardCowState`] guard-protected
/// back to its own real prior protection, and clears [`GUARD_STATE`] to `None` -- see
/// [`try_claim_guard_cow_for_fork`]'s own doc comment (88th-pass fix) for why this must run BEFORE
/// a newly-reclaimed slot's own [`reserve_group_lazy_guarded`] calls can ever re-guard the same
/// address range. A region already healed by [`guard_cow_write_fault_veh`] (its own page long
/// since restored to `old_protect` and a snapshot captured) is harmlessly re-`VirtualProtect`'d to
/// the exact same value it already holds -- idempotent, no different from any other region here.
fn heal_superseded_guard_state() {
    // Lock order (GUARD_STATE, then VIRTUAL_PROTECT_LOCK) deliberately matches
    // `guard_cow_write_fault_veh`'s own -- held together, not dropped-then-reacquired, so no
    // OTHER thread's write fault on one of these about-to-be-healed regions can observe
    // `GUARD_STATE` as `None` (declining the fault, falling through to a handler that does not
    // know what to do with it) in the narrow window between clearing this state and actually
    // restoring the page protection. No other code path takes these two locks in the opposite
    // order (`reserve_group_lazy_guarded` takes only `VIRTUAL_PROTECT_LOCK`, never `GUARD_STATE`),
    // so holding both here cannot deadlock against it.
    let mut guard_state = GUARD_STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(state) = guard_state.take() else {
        return;
    };
    let _vp_guard = crate::VIRTUAL_PROTECT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for region in &state.regions {
        let mut discard_old: u32 = 0;
        unsafe {
            VirtualProtect(
                region.range.start as *mut c_void,
                region.range.len(),
                region.old_protect,
                &mut discard_old,
            );
        }
    }
}

/// Releases a claim that ended up unused (spawn failed before any group was guarded, or this
/// fork turned out to have zero lazy-eligible groups after all). Only ever resets the slot from
/// the placeholder -- never stomps a value a (impossible, single-claimant) different finalize
/// already wrote.
fn guard_cow_release_claim() {
    let _ = GUARD_COW_OWNER_PID.compare_exchange(
        GUARD_COW_CLAIM_PLACEHOLDER,
        0,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
}

/// Finalizes a successful claim once the real child pid is known (spawn already succeeded) and at
/// least one group has actually been guarded -- publishes the real owner pid (so a FUTURE claim's
/// liveness check targets the right process) and installs [`GUARD_STATE`].
fn finalize_guard_cow_claim(child_pid: u32, regions: Vec<GuardedRegion>, table: &'static [GuardSnapshotSlot]) {
    GUARD_COW_OWNER_PID.store(child_pid, Ordering::SeqCst);
    let mut state = GUARD_STATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    *state = Some(GuardCowState { regions, table });
    drop(state);
    ensure_guard_cow_veh_installed();
}

fn ensure_guard_cow_veh_installed() {
    GUARD_VEH_INSTALLED.get_or_init(|| {
        unsafe {
            AddVectoredExceptionHandler(1, Some(guard_cow_write_fault_veh));
        }
    });
}

/// Parent-side setup, called once per lazy-eligible group when [`try_claim_guard_cow_for_fork`]
/// succeeded for this fork: reserves the group in the child exactly as [`reserve_group_lazy`]
/// does, THEN walks this PARENT's own already-committed pages across `source_group`
/// `VirtualQuery`-region by region, `VirtualProtect`-ing each committed, non-guard/no-access
/// sub-range to `PAGE_READONLY` and recording it as a [`GuardedRegion`] (with its OWN real prior
/// protection, and a slot-base index computed relative to `group_slot_base`, matching the child's
/// own [`GUARD_GROUP_BASES`] computation for the same group).
///
/// A sub-range whose `VirtualProtect` call itself fails is simply left unguarded (not fatal to the
/// whole fork) -- the affected pages fall back to the pre-existing plain-live-read path for lazy
/// faults, which is Bug 4's original TOCTOU risk narrowed to just that sub-range, never a NEW
/// hazard beyond what already existed before this pass.
#[must_use]
fn reserve_group_lazy_guarded(
    child: Handle,
    source_group: &Range<usize>,
    group_slot_base: usize,
    regions_out: &mut Vec<GuardedRegion>,
) -> GroupCopyResult {
    let result = reserve_group_lazy(child, source_group);
    if !result.succeeded {
        return result;
    }
    let _guard = crate::VIRTUAL_PROTECT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            let mut old_protect: u32 = 0;
            let protected = unsafe {
                VirtualProtect(
                    region_start as *mut c_void,
                    region_end - region_start,
                    PAGE_READONLY,
                    &mut old_protect,
                ) != 0
            };
            if protected {
                regions_out.push(GuardedRegion {
                    range: region_start..region_end,
                    old_protect,
                    slot_base_index: group_slot_base + (region_start - source_group.start) / PAGE_SIZE,
                });
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
    regions: Vec<GuardedRegion>,
}

/// Parent-side, PRE-spawn: attempts to claim this process's single guard-cow slot (see
/// [`try_claim_guard_cow_for_fork`]) and, only if successful, allocates (and `'static`-leaks) a
/// snapshot table sized for `total_pages` guest pages -- the sum, across every group this fork
/// intends to guard, of `group.len().div_ceil(PAGE_SIZE)`. Returns `None` on a declined claim;
/// the caller must then fall this fork all the way back to eager for every group (never plain
/// unguarded lazy) -- see this module's own doc comment for why that is the deliberately
/// conservative choice.
#[must_use]
pub fn try_claim_guard_cow_table(total_pages: usize) -> Option<GuardCowClaim> {
    if !try_claim_guard_cow_for_fork() {
        return None;
    }
    let table: Vec<GuardSnapshotSlot> = (0..total_pages).map(|_| GuardSnapshotSlot::zeroed()).collect();
    let table: &'static [GuardSnapshotSlot] = Box::leak(table.into_boxed_slice());
    Some(GuardCowClaim {
        table,
        regions: Vec::new(),
    })
}

/// The table's base address, to serialize into [`FORK_CHILD_GUARD_COW_TABLE_ENV_VAR`].
#[must_use]
pub fn guard_cow_claim_table_base(claim: &GuardCowClaim) -> usize {
    claim.table.as_ptr() as usize
}

/// Parent-side, POST-spawn (needs the real child `Handle`): reserves+guards one lazy-eligible
/// group under an already-successful [`GuardCowClaim`] -- see [`reserve_group_lazy_guarded`].
#[must_use]
pub fn guard_cow_reserve_group(
    claim: &mut GuardCowClaim,
    child: Handle,
    source_group: &Range<usize>,
    group_slot_base: usize,
) -> GroupCopyResult {
    reserve_group_lazy_guarded(child, source_group, group_slot_base, &mut claim.regions)
}

/// Commits a successful claim: publishes the real child pid as this slot's owner (so a FUTURE
/// claim's liveness check targets the right process) and installs [`GUARD_STATE`] so
/// [`guard_cow_write_fault_veh`] actually starts servicing faults for these regions.
pub fn finalize_guard_cow_table(claim: GuardCowClaim, child_pid: u32) {
    finalize_guard_cow_claim(child_pid, claim.regions, claim.table);
}

/// Abandons a claim that never became a real, running guarded fork (the child's own spawn or
/// per-group copy loop failed). Restores every region THIS claim already guard-protected back to
/// its own real prior protection before releasing the slot -- without this, a partially-guarded
/// set of pages would stay stuck `PAGE_READONLY` in the parent forever with no
/// [`GUARD_STATE`]-installed handler ever able to heal them (this claim was never finalized, so
/// [`guard_cow_write_fault_veh`] never learns about these regions at all).
pub fn abort_guard_cow_claim(claim: GuardCowClaim) {
    let _guard = crate::VIRTUAL_PROTECT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for region in &claim.regions {
        let mut discard_old: u32 = 0;
        unsafe {
            VirtualProtect(
                region.range.start as *mut c_void,
                region.range.len(),
                region.old_protect,
                &mut discard_old,
            );
        }
    }
    drop(_guard);
    guard_cow_release_claim();
}

/// PARENT-side VEH: catches this process's OWN next write to a page it has guard-protected on
/// behalf of a lazy fork child, captures a pre-write snapshot, republishes the region's real prior
/// protection on just that one page, and lets the write retry and succeed. Declines
/// (`EXCEPTION_CONTINUE_SEARCH`) every fault that is not a write to a currently-guarded address,
/// including every fault this process's own pre-existing `fork_verify`/main VEH machinery is
/// responsible for -- unchanged by this handler's mere presence.
unsafe extern "system" fn guard_cow_write_fault_veh(info: *mut EXCEPTION_POINTERS) -> i32 {
    // Cheap, lock-free bail-out before touching any lock: guard-cow inactive for this process
    // right now (the overwhelmingly common case -- this handler, once installed, stays installed
    // for the rest of this process's life, but is active only while GUARD_STATE is Some).
    if GUARD_COW_OWNER_PID.load(Ordering::SeqCst) == 0 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
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

    let mut guard_state = GUARD_STATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(state) = guard_state.as_mut() else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let Some(region_idx) = state.regions.iter().position(|r| r.range.contains(&fault_addr)) else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let page_addr = fault_addr & !(PAGE_SIZE - 1);

    // Hold VIRTUAL_PROTECT_LOCK for the whole query-flip-copy-restore span -- the exact precedent
    // `fork_verify::write_usize_fault_tolerant` already established for taking this lock from
    // inside VEH dispatch (see this module's own doc comment, "87th pass" section, finding 3).
    let _vp_guard = crate::VIRTUAL_PROTECT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let region = &state.regions[region_idx];
    let slot_index = region.slot_base_index + (page_addr - region.range.start) / PAGE_SIZE;
    let old_protect = region.old_protect;
    let Some(slot) = state.table.get(slot_index) else {
        return EXCEPTION_CONTINUE_SEARCH;
    };

    // Idempotent: a second concurrent write fault to the SAME page (another thread, blocked on
    // VIRTUAL_PROTECT_LOCK above until the first finishes) finds state already 1 and simply skips
    // straight to the restore-and-retry step below -- no double-capture, no lock re-entrancy.
    if slot.state.load(Ordering::Acquire) == 0 {
        unsafe {
            core::ptr::copy_nonoverlapping(
                page_addr as *const u8,
                (*slot.bytes.get()).as_mut_ptr(),
                PAGE_SIZE,
            );
        }
        slot.state.store(1, Ordering::Release);
        if std::env::var_os("LITEBOX_DIAG_LAZY_FORK_COMMIT").is_some() {
            eprintln!(
                "[lazy_fork_commit] guard-cow: parent write-fault captured page={page_addr:#x} slot={slot_index}"
            );
        }
    }

    let mut discard_old: u32 = 0;
    let restored = unsafe {
        VirtualProtect(
            page_addr as *mut c_void,
            PAGE_SIZE,
            old_protect,
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
