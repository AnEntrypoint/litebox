# AGENTS.md archive, 2026-09-28 (drained at the 117th pass)

Verbatim: the pre-compaction 'current state' header (114th-pass era) and the full pass-history section 4th-116th. Read for a trail only; the live picture is AGENTS.md.

---

﻿# litebox -- current state (2026-09-24)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, or one a later commit superseded, is deleted rather than hedged. Reference
detail is drained to `docs/AGENTS_ARCHIVE_*.md` and dated `docs/*.md` in the map below -- read those
for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one
line plus its pointer, not a separate memory file. Compacted at the 65th, 70th, 72nd, 75th, 76th,
81st, 83rd, 85th, 88th, 91st, 93rd, 95th, 98th, 101st, 104th and 106th passes (pass-history section
below; 26th-69th full narrative: `docs/AGENTS_ARCHIVE_2026-09-22.md`; 70th-105th full narrative,
including each pass's own complete evidence and fix rationale: `docs/AGENTS_ARCHIVE_2026-09-23.md` --
every condensed bullet below cites this same archive for its own full narrative). The 106th pass's
compaction took this from 74.3KB to ~54KB by draining the 103rd-105th passes' own full narrative to
the archive -- still over the 30KB target; "Standing lessons"/"Docs and tooling map"/"Closed" are the
remaining drain candidates for a future pass (the pass-history bullets above this line are already
condensed close to the point of losing load-bearing detail; further cuts there risk exactly the
"claim nobody could point at" failure mode this file's own opening paragraph warns against).

**Where things stand, in one paragraph (updated 113th pass; comm-inheritance fix and new negative
evidence added by the 114th pass, see its own pass-history entry below)**: three real gaps that used to
force every cross-process fork onto the crash-prone thread-based relocating path are now closed --
109th-112th passes landed cross-process `kill()`/process-groups/`SIGCHLD` siginfo, a single shared
data path for ptys (Ctrl-C, `script`, interactive terminals all verified), and unix-socket carrying
(connected streams, socketpairs, listeners) -- real boots now show **zero** `not eligible` fallbacks
(`.wfgy/pass112_boot{1,2,3}.err.log`). The 103rd pass's `xfce4-session: Cannot open display: .` +
immediate `exit(1)` **is a RACE, not a deterministic bug** (envp proven correct at `execve` via a
new permanent diagnostic, `[diag-xfce4session-envp]`), and it is CLOSED for the non-lazy path: two
harness bugs caused it -- `de_only.sh`'s `XSOCK_WAIT_DONE` only checked the socket FILE's existence,
not Xvfb's actual connection-accepting readiness (fixed: `PROBE_XSET` now retries its own connect
probe up to 15s); and `xrdb "$HOME/.Xresources"` ran through xrdb's default cpp-preprocessing pass
for a one-line file with nothing to preprocess, forking a real `sh -> cpp -> cc1` chain that
directly caused a live ENOMEM crash (fixed: `xrdb -nocpp`). Both fixes are baked into a new seed,
`.wfgy/pass113_de_only_ready_seed.tar` (built from `pass103_de_only_trimmed_seed.tar` -- use this
one, not the older `_trimmed_` seed, for all future boots). **With both fixes, non-lazy
(`LITEBOX_PROCESS_FORK=1` alone) is now CORRECTNESS-CLEAN END TO END**: `xfce4-session` forks
`ssh-agent`/`iceauth`/`xfwm4`/`xfsettingsd` with zero crashes across every run this pass, confirmed
by a real syscall-level trace (`.wfgy/pass113_sshagent_hang2.err.log`) showing `ssh-agent` exit
cleanly and `xfwm4`'s own `execve` succeed (`.wfgy/pass113_nocpp_boot1.err.log`,
`pass113_final_denup.err.log`). **The sole remaining blocker on the non-lazy path is Track B item
1's own RAM crater** -- every run so far reaches `WM_POLL n=3`-`n=5` (~115-130s) before free RAM
falls below the safety kill-switch (15-17 concurrent processes), just short of `xfwm4` finishing its
own startup long enough to set `_NET_SUPPORTING_WM_CHECK`. This is now purely a resource/timing
question, not a correctness one: a genuinely sustained 6GB+-free run (no large unrelated host
process competing) is very plausibly enough on its own. **`LITEBOX_LAZY_FORK_COMMIT=1
LITEBOX_LAZY_FORK_GUARD_COW=1` (the mechanism that actually avoids the RAM crater) DOES avoid it --
confirmed live, `.wfgy/pass113_lazy_final.out.log` ran the full 200s `WM_POLL` window (n=1..20) with
RAM stable and never cratering -- but has a REAL, DISTINCT, STILL-OPEN correctness bug**, found for
the first time in a genuine desktop-boot shape rather than a synthetic repro: `xfce4-session`'s own
fork of its next session client (right after `ssh-agent` exits and is `wait4`'d) aborts with a
`SIGABRT` before ever reaching `execve` -- guest pid 51 / winpid 2132, comm still blank at the
moment of death, `fatal signal: terminating task signal=Signal(6)`, ~0.17s after "entering real
guest execution" (`.wfgy/pass113_sshagent_hang.err.log:16232`, `pass113_lazy_final.err.log`). This
is the SAME TOCTOU/Bug-4 correctness class `lazy_fork_commit.rs`'s own doc comment has documented
since the 85th pass (a lazy-serviced page reading the parent's CURRENT, not fork-time, memory) --
the concurrent-claim-cap mitigation (102nd pass) reduces its FREQUENCY but was never proven to
CLOSE it, and this is the first time it has actually recurred in the real target workload rather
than a synthetic subshell. **`DE_UP` has not been reached by any of the 113 passes to date. Both
lazy-fork flags remain default OFF.** Recorded as tracked defects in `.gm/prd.yml`:
`non-lazy-fork-ram-crater-before-de-up` (resource/timing, non-lazy) and the pre-existing
lazy-fork-commit TOCTOU item (rescoped to include this new real-workload repro). **Next pickup**:
(a) non-lazy -- **corrected, live-measured**: 6GB free at boot start is NOT enough on its own --
two independent runs starting at 6.09-6.36GB free both cratered at the identical `WM_POLL n=4`
point (~130s elapsed, 14->15->17 processes, 0.6-0.8GB free) with zero correctness issue either
time. The boot's own cumulative committed-memory need by this point is a real, consistent ~5GB+
regardless of starting headroom in this range -- needs either more like 8-10GB+ free sustained, or
a genuine reduction in Track B item 1's own cumulative cost (the original, still-open 76th-82nd
pass investigation). This needs no further code changes, only more host RAM than has been
available this session. (b) lazy -- **negative evidence gathered, real trigger still not isolated
to a cheap repro**: neither a sequential fork-no-exec/`wait4`/fork-no-exec-again pattern nor a
genuinely CONCURRENT two-outstanding-children pattern (both on `debian:stable-slim`,
`.wfgy/pass113_sigabrt_repro.sh`/`pass113_concurrent_repro.sh`) reproduces the crash -- both ran
clean, no `Signal(6)`, both children exited normally. The real trigger likely needs the SPECIFIC
shape only the real boot has: `xfce4-session`'s own vfork'd `/bin/sh`->`iceauth` chain (shares the
SAME Windows process as `xfce4-session` itself, unlike an ordinary cross-process sibling) still
alive/untracked-as-reaped at the moment a THIRD, genuinely cross-process child is forked -- **this
hypothesis is now REFUTED by careful re-reading, not just untested**: real vfork semantics (and
this codebase's own implementation, `Process::wait_for_vfork_done`) block the PARENT's own thread
entirely until the vfork child calls `execve`/exits, and `detach_pm_for_vfork_execve` gives the
`execve`'ing child (`iceauth`) a brand-new, fully-detached `PageManager` at the START of its own
`sys_execve`, before touching memory -- so by the time `xfce4-session`'s thread is even running
again (to fork the crashing child), the vfork sharing has ALREADY ended and nothing else has any
claim on its memory. Drop this angle; it does not explain the crash.
  - **Real, general (non-crash-specific) gap found while investigating, ROOT-CAUSED AND FIXED
    THIS PASS (114th)**: the 113th pass's own claim that "`comm` is NEVER copied from the parent at
    fork time" was too general and is corrected here -- `do_clone`'s real thread-based clone path
    (`litebox_shim_linux/src/syscalls/process.rs:5103`) already did this correctly
    (`comm: self.comm.clone()`, with an explicit doc comment at line 5148 confirming real-Linux
    semantics). The actual, narrower bug was specific to the OTHER Task-construction path: a
    cross-process fork child's own `Task` is built fresh, with no parent `Task` in the same OS
    process to copy from, by `LinuxShim::adopt_forked_process`
    (`litebox_shim_linux/src/lib.rs:1062`) -- and THAT function unconditionally hardcoded
    `comm: [0; TASK_COMM_LEN].into()`, discarding whatever the parent was actually named, on EVERY
    cross-process fork (i.e. every fork under the recommended `LITEBOX_PROCESS_FORK=1` path -- the
    dominant one in this whole 113-pass investigation). This is exactly why every `DIAG_TIMELINE
    clone`/`execve` line for a forked child showed a blank `comm`, and exactly why
    `LITEBOX_DIAG_SYSCALL_TIMELINE`'s comm-based filter could never see a forked child's own
    pre-`execve` syscalls (the blind spot that made the 113th pass's "zero syscalls ever traced for
    the crashing pid" finding meaningless). **Fixed by threading the parent's real `comm` bytes
    through the same way `sigreturn_trampoline` already was** (same precedent, same
    `CreateProcessW`-environment-variable-export mechanism): added `comm: [u8; 16]` to
    `litebox::platform::PlatformExtensions::spawn_cross_process_fork_child`'s trait signature
    (`litebox/src/platform/mod.rs`), threaded it through the Windows impl
    (`litebox_platform_windows_userland/src/lib.rs`) and `spawn_process_fork_child`
    (`process_fork.rs`, new `FORK_CHILD_COMM_ENV_VAR` = `LITEBOX_INTERNAL_FORK_CHILD_COMM`,
    hex-encoded to survive the env-var boundary), read it back in the child-side bootstrap
    (`litebox_runner_linux_on_windows_userland/src/lib.rs`'s `diag_process_fork_task_resume_probe`
    -- confirmed, despite its `diag_` name, to be the REAL production Task-construction path for
    every cross-process fork child: `spawn_process_fork_child`'s own doc comment says the three
    `LITEBOX_DIAG_PROCESS_FORK_*` gates it sets are "always" set, unconditionally, on this
    production path), and passed it into `adopt_forked_process`'s now-required `comm` parameter.
    **Live-verified** (`.wfgy/pass114_comm_repro.sh`/`.ps1`, cheap `debian:stable-slim` + `/bin/bash
    -s` repro, `LITEBOX_PROCESS_FORK=1` alone, no lazy flags): `DIAG_TIMELINE execve pid=2 ppid=1
    comm=[98, 97, 115, 104, 0, ...] argv0=/usr/bin/sleep` -- `[98,97,115,104]` decodes to `"bash"`,
    the forked child's real parent name, exactly where a blank `[0,0,...]` array appeared before
    this fix. Added the pid-based `LITEBOX_DIAG_SYSCALL_TIMELINE_PID` filter (113th pass) remains
    useful as a belt-and-suspenders option, but the comm-based filter itself is now trustworthy
    again for cross-process-forked children too, not just thread-based ones. Not yet re-attempted
    against the actual lazy-fork-commit SIGABRT crash (guest pid 51/winpid 2132) -- that is the
    immediate next pickup now that the tooling blind spot is genuinely closed rather than routed
    around.
  - **Also tried and correctly abandoned this pass**: `LITEBOX_DIAG_PROCESS_FORK_EXTERNAL_DEBUGGER`
    (a pre-existing, 143rd/144th-pass kernel-debug-event observer) plus a new
    `LITEBOX_DIAG_PROCESS_FORK_EXTERNAL_DEBUGGER_SKIP=<n>` gate added this pass to scope it to only
    the fork under investigation. It correctly passes exceptions through to VEH
    (`DBG_EXCEPTION_NOT_HANDLED`, confirmed by re-reading `observe_real_resume_fault` -- an EARLIER
    draft of this same paragraph wrongly claimed it bypasses VEH via `DBG_CONTINUE`; that claim was
    corrected in the same pass, `162fe02`, rather than left standing), but its own real per-event
    overhead (a `WaitForDebugEvent` round trip, `GetThreadContext`, a multi-field `eprintln!`, per
    fault) was enough to measurably worsen the RAM crater on a lazy-fork-commit boot (which can
    legitimately take many deliberate page faults during ordinary startup) -- confirmed by direct
    comparison against the clean, undiagnosed `pass113_lazy_final` run. Kept the `_SKIP` gate
    (harmless when unset, real use for a genuinely unrecoverable non-lazy crash where this overhead
    doesn't matter), but it is not the right tool for THIS specific investigation.
  - **Next pickup, precise**: once host RAM is genuinely, sustainedly free (the 8-10GB already
    established as insufficient at 6GB doesn't even apply here -- this pass never even reached 5GB
    sustained), re-run targeting the crashing lineage's own comm (now genuinely trustworthy across
    the fork boundary post-114th-pass-fix -- e.g. `LITEBOX_DIAG_SYSCALL_TIMELINE=<comm>`, the
    ORIGINAL, simpler mechanism, not just the pid-based workaround) to finally see the crashing
    child's OWN pre-`execve` syscalls, which no capture has ever shown before. The pid-based
    fallback (`LITEBOX_DIAG_SYSCALL_TIMELINE_PID=48,49,50,51,52,53` or whatever pids a fresh
    `DIAG_TIMELINE clone`/`execve` sequence shows for `xfce4-session`'s own children on that specific
    run, `.wfgy/pass113_pidtrace.ps1`) remains available as a belt-and-suspenders cross-check.


---

**Pass history (4th-107th, 2026-09-17/23)**: full narrative for every pass below is in the dated
archives ("Docs and tooling map" below) — these bullets are already condensed; do not re-condense
further without re-reading the archive first (the "claim nobody could point at" risk this file's own
opening paragraph warns about).

- **4th-42nd**: `docs/AGENTS_ARCHIVE_2026-09-22.md`. Fork fd eligibility, OCI cache, s6-boot,
  cross-process fork's initial `D==0` design and its first bug wave (guest-mmap alignment, stdio
  handles, `fd/mod.rs:422`).
- **43rd-74th**: `docs/AGENTS_ARCHIVE_2026-09-22.md`/`_2026-09-23.md`. FIXED/REFUTED, live-verified:
  both Xvfb SIGSEGVs; D-Bus activation's dropped-CLOEXEC-fd bug; per-fork rootfs-rebuild RAM cost
  (56th); `ssh-agent`/`xfwm4` permanent freeze (`RawMutex::WaiterQueue::with_lock`, 60th/61st);
  `SharedUnixConnectQueue::cancel` slot leak (62nd); `DBUS_FAILED` (67th/68th). REFUTED: `/defaults/
  xfce/` readdir, dbus babysitter SIGKILL, epoll-readiness, GLX/compositor theories. 70th-74th:
  root-caused two logging gaps hiding `xfwm4`'s own X11 traffic; added `litebox_diag::
  process_timeline`/`socket_read`. `DE_FAILED`/RAM collapse survived all of it.
- **75th-82nd**: `docs/AGENTS_ARCHIVE_2026-09-23.md`. 75th: **`xfwm4` launches for the first time
  ever**, FIXED (`1d449e6`) a writable-layer export-path bug breaking filesystem-write visibility on
  every boot. 76th: admission control (`live_cross_process_fork_children`, caps 6) — real but
  partial. 77th: FIXED (`621ee1a`) a real 2x host-allocator commit-doubling bug — not sufficient
  alone. 78th-82nd: measured (not guessed) that fork-then-immediate-`execve()` wastes ~85% of cycle
  time on eager copy, ruled out cap-tuning and two deferred-copy shortcuts (a second correctness
  obstacle: a plain `fork()` child may legally write memory pre-`execve`), converging on genuine
  per-page lazy population as the only remaining lever.
- **83rd-87th**: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "83rd-87th pass full narrative". Implemented
  lazy reserve-then-commit-on-fault fork memory (`lazy_fork_commit.rs`,
  `LITEBOX_LAZY_FORK_COMMIT=1`) — real win for fork-then-`execve`; fixed 3 bugs along the way (guest-
  mmap alignment collision, sigreturn-trampoline non-inheritance, an active-`%rsp`-group bug); found
  but did NOT fix Bug 4 (TOCTOU: a lazily-serviced fault reads the parent's CURRENT memory, unsafe
  for fork-without-`execve`); designed the single-generation guard-page-COW fix. Both flags default
  OFF throughout; `DE_UP` not attempted.
- **88th-90th**: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "88th-90th pass full narrative". 88th:
  IMPLEMENTED guard-page COW (`LITEBOX_LAZY_FORK_GUARD_COW=1`), fixed a guard-cow hang (Bug 5),
  reached furthest yet (`WM_POLL` → a real X window) before `DE_FAILED`. 89th: root-caused
  `DE_FAILED` to a live `xfce4-session` `STATUS_ACCESS_VIOLATION` crash, unrelated to lazy/guard-cow.
  90th: root-caused it to a genuine `CLONE_VFORK`, fixed two architectural bugs (blind fresh
  `PageManager` risking live-parent-memory corruption on `execve`; a `release_memory`/
  `Vmem::duplicate` regression that fix itself caused) — both live-verified fixed, but the ORIGINAL
  crash itself survived unchanged into the 91st pass.
- **91st-97th**: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "91st-94th"/"95th-97th pass full narrative" —
  **a 7-pass misattribution, resolved.** 91st-96th chased a "Windows clears `GS_BASE`/`FS_BASE` under
  scheduling pressure" theory (fixed several real, general `RawMutex` register-repair gaps along the
  way; kept landed) to a decisive negative result. **97th overturned the theory entirely**: the real
  crash is a plain `CLONE_THREAD` pthread hit by the already-documented Bug 4 TOCTOU, confirmed via a
  clean A/B — the whole FS_BASE/GS_BASE chase, while each fix is real, was chasing a misattributed
  symptom of Bug 4. Also fixed: `VEH_FRAME_STRIDE`/`EXCEPTION_RECORD_RESERVE` sized only for release
  codegen, crashing every debug build — widened 8x under `#[cfg(debug_assertions)]`.
- **98th-101st**: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "98th-100th pass full narrative". 98th
  generalized Bug 4's fix to N concurrent per-parent generations (`GUARD_PAGE_REGISTRY`). 99th found
  a real 100%-reproducible regression (crater at 20s/7 procs; new `rc=134` heap corruption). 100th
  root-caused `rc=134` to Bug 7 (`sys_execve` never called `disarm_on_execve`) and fixed it plus two
  more (Bug 6a: `OpenProcess` handle churn; Bug 6b: `GUARD_PAGE_REGISTRY`/`mprotect` desync) — `rc=134`
  gone, but an older `rc=139` SIGSEGV resurfaced, crater speed unchanged. 101st implemented the
  batched-`VirtualProtect` optimization (real win, kept landed) but confirmed by A/B it does NOT fix
  crater speed (the crater is `VirtualAlloc2(MEM_COMMIT)` charge, not guard-cow overhead) — deferred
  `rc=139` to a live `cdb` session. Both flags default OFF throughout; `DE_UP` not reached.
- **102nd**: root-caused+FIXED the crater-speed regression (99th-101st) by code reading: every
  guard-cow claim `Box::leak`s a full-page table, and 98th's N-concurrent generalization removed an
  incidental rate limit the old single-owner gate provided. Fix: `GUARD_COW_CONCURRENT_CLAIM_CAP: u32
  = 3` (`lazy_fork_commit.rs`) restores it. Verified: isolated repro + real boot A/B (crater-to-
  kill-switch → stable ~188s run, matching the pre-98th baseline). `rc=139` reconfirmed real and
  unaffected (not root-caused this pass; superseded by 103rd/104th below).
- **103rd-105th**: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "103rd-105th pass full narrative". 103rd:
  first pass to directly investigate `DE_FAILED` — root-caused it on both configs (lazy:
  `xfce4-session` dies of `STATUS_ACCESS_VIOLATION`; non-lazy: RAM crater); found the cheap
  `Xvfb`+`xset q` repro for the same crash class (17/25 faults at `0x7feffffef000`). 104th: FIXED one
  real cause — the sigreturn-trampoline page could be merged into a lazy-eligible group,
  infinite-refaulting its own deliberate trap (`classify_lazy_eligible_groups` fix) — zero
  `0x7feffffef000` post-fix, but the same repro still showed `Signal(11)`/`rc=139`, traced to
  `fork_verify.rs`'s OWN independent healer hitting the same address. 105th: FIXED that second
  instance (threaded `sigreturn_trampoline_addr()` into `fork_verify.rs`'s heal-decline check) — 20
  occurrences → 0 post-fix. **Still NOT closed**: a THIRD, distinct bug (6 fatal `Signal(11)` events,
  zero trampoline hits) — clearest crash follows `DIAG_TIMELINE execve argv0=/usr/bin/rm`. Both lazy
  flags stay default OFF.
- **106th-107th**: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "106th-107th pass full narrative" (the
  deterministic-address evidence, the refuted CoW-mmap hypothesis, the `allocate_pages` near-miss
  analysis). Investigated the 105th pass's open `/usr/bin/rm` crash (a deterministic guest `#PF` at
  `cr2=0x111156f60`, litebox's `Vmem` believing the page present while Windows backing was not) by
  code reading and log re-mining, ruling out `allocate_pages`, the CoW-mmap fast path, and a stale
  VMA-adoption diagnostic filter — did not find the mechanism. **Superseded**: the 109th pass fixed
  this exact crash from a completely different angle (`sys_execve`'s disarm-ordering) — see below.
- **109th-113th (full narrative drained to `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "109th-113th pass
  full narrative" section)** -- closed the three real gaps that used to force every cross-process
  fork onto the crash-prone thread-based path. 109th: fixed the crash the 102nd/106th/107th passes
  chased -- `sys_execve` was disarming `lazy_fork_commit`'s lazy servicing at function ENTRY,
  before `copy_vector` finished reading the OLD program's still-lazy `argv`/`envp` (`24cb72d`); also
  fixed a false-alarm AF_UNIX presence-miss WARN (`d16e5ce`). 110th: cross-process `kill()`/
  process-groups/`SIGCHLD` (`SharedProcessTable`, `xproc.rs`), plus pty `ISIG`/slave-carrying; found
  and fixed a real guest-pid-vs-Windows-pid identity bug along the way. 111th: pty data path
  unified onto `SharedPtyTable` for every process (no more per-process-local channel gap); `SIGCHLD`
  now carries real child siginfo. 112th: unix sockets (connected streams, socketpairs, listeners)
  carried across a cross-process fork -- eliminates the last thread-path fallback; fixed a dead-
  slot-reclaim bug and a writable-layer-import abort-on-first-failure bug along the way. 113th:
  fixed a real diagnostic bug (`is_syscall_timeline_target_comm` matched trivially on an empty
  `comm`, hiding every forked child's pre-`execve` syscalls) and confirmed `xfce4-session`'s
  "Cannot open display: ." is a **timing race**, not a deterministic bug -- same script, same code,
  clean run vs. crash run. **Net result of 109th-113th**: real boots now show zero `not eligible`
  fallbacks, and non-lazy (`LITEBOX_PROCESS_FORK=1` alone) is correctness-clean end to end -- the
  sole remaining blocker on that path is Track B item 1's own RAM crater (below), a resource/timing
  question now, not a correctness one.
- **114th (full narrative drained to `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "114th pass full
  narrative" section)** -- fixed the comm-inheritance bug (top paragraph, committed, live-
  verified). Built a synthetic multi-threaded-parent fork stress test and found a genuine NEW
  host-level `STATUS_ACCESS_VIOLATION` (distinct from the pre-existing guest-level Bug 4 TOCTOU),
  reproducible under `LITEBOX_LAZY_FORK_GUARD_COW=1` in under 2 seconds, no full boot needed
  (`.wfgy/pass114_torn_read_probe.sh`). Refuted three theories in a row by direct rebuild+test
  (per-page guard-install timing; a `__chkstk` stack-overflow, fixed regardless via a
  `thread_local!` scratch buffer, `FAULT_SCRATCH_BUF`; and an INITIALLY-plausible sigreturn-
  trampoline main-handler correlation, refuted once real pid-tagging was added -- pure cross-
  process log interleaving). Installed `cdb` (user-approved) and, after working around several
  `-c`-script-timing/quoting obstacles (full detail: archive), got a live, `!address`-verified
  capture: the crashing GUEST thread's own stack page is `MEM_COMMIT`/`PAGE_READONLY` with
  `Allocation Protect: PAGE_NOACCESS` -- a real, committed page stuck read-only, so its very first
  ordinary write crashes. Theorized mechanism: a stale guard-cow claim's `PAGE_READONLY` from an
  earlier, since-dead fork generation, never healed because nothing outside `guard_one_page`'s own
  per-page claim path ever re-checks a page once it's no longer being actively guarded. Implemented
  `heal_stale_guard_entries_in_range` (called from both `allocate_pages` success paths, real
  hardening, kept) but **live re-test confirmed it does NOT fix this crash** -- no registry entry
  existed for this page at fault time, so the theorized mechanism is wrong or incomplete. Also
  chased and REFUTED a "mystery small-stack internal thread" theory (implemented+tested twice, no
  effect) that was itself based on a red herring: named every litebox-internal background thread
  (`ctxwatch.rs`'s DR1 helper, `net.rs`'s TCP-flow-connect helper, `lib.rs`'s fork-pipe-pump/
  xproc-exit-notifier/signal-wake-listener threads, all now `litebox-*`-named; `spawn_thread` now
  names every GUEST thread `litebox-guest-pid<N>`) and re-captured: the crashing thread is
  `"litebox-guest-pid2"`, an ORDINARY guest thread (its tiny TEB-tracked stack is irrelevant --
  litebox runs guest code with `%rsp` pointing at guest-mapped memory, never the host thread's own
  TEB stack, by design). By end of pass, code reading had ruled out every candidate call site
  found by inspection (`guard_one_page`, `try_guard_region_batched`, `invalidate_guarded_range`,
  `heal_stale_guard_entries_in_range`, `lazy_commit_veh`, `reserve_and_commit`/`prot_flags`) --
  none can produce this exact `PAGE_READONLY` signature for an ordinary thread-stack `mmap`. Also
  confirmed (by direct code reading, not assumption) that a genuine negative finding this pass --
  zero `[lazy_fork_commit]` diagnostic output despite the guard-cow-like signature -- is NOT
  explained by a stdio- or env-inheritance gap for the cross-process-forked child (both verified
  correctly wired); the real explanation remains unconfirmed. **Five consecutive attempts at a
  live `cdb` breakpoint capture on `VirtualProtect`/`VirtualAlloc2` for the crash address each hit
  a genuinely new cdb-scripting obstacle** (`-g`-deferred script timing; an invalid `sxd ibp` event
  name; `&&` not parseable in `.if`; and a `g`-inside-a-`$$<`-loaded-file failure reproduced twice,
  once recursively and once fully unrolled, root mechanism not identified) -- full blow-by-blow in
  the archive. **Declined a sixth blind attempt**: a future session should go interactive (a real
  `cdb` window, not scripted/redirected) or solve the original quoting problem without a `$$<file`.
  **Net status**: crashing thread identity confirmed (ordinary guest thread); root mechanism of
  the `PAGE_READONLY` signature still NOT confirmed after ruling out every code-reading candidate;
  do NOT ship a blind fix; the trampoline-collision loop and this STATUS_ACCESS_VIOLATION remain
  two DIFFERENT, unresolved bugs; `xfce4-session`'s own original SIGABRT remains unre-attempted.
- **115th -- compacted this file (78.8KB -> 53.1KB, full 114th-pass narrative drained to the
  archive); tried a `cdb`-free repro (litebox's own always-reliable `eprintln!` diagnostics,
  `LITEBOX_DIAG_MM=1` + `LITEBOX_DIAG_LAZY_FORK_COMMIT=1`, no debugger at all) to sidestep the
  114th pass's cdb-scripting dead end -- found a new, real, but still-unconfirmed data point,
  not a root cause.** 2/2 plain (non-cdb) runs of the exact same repro end in the guest-visible
  process getting reported as killed by SIGKILL (bash's own `$?`=137 after the `python3 -`
  heredoc) within ~7s, both with `LITEBOX_DIAG_NO_FAULT_WATCHDOG`/`_NO_EXTERNAL_FAULT_WATCHDOG`
  set (ruling out both known internal watchdogs as the cause -- confirmed no third watchdog gate
  exists by grepping every `WATCHDOG`-related env var in the codebase) and with
  `killed_by_ram_switch: False` confirmed by the driving script itself (not an external kill from
  this session's own tooling either). **No crash dump of any kind appears** -- `.wfgy/
  pass115_plain_repro{2,3}.err.log` end abruptly on an ordinary `[lazy_fork_commit]` line with
  nothing after, despite AGENTS.md's own standing claim that "a fatal host fault dumps before it
  dies, ungated... no env var needed." Plausible explanation, not yet confirmed: the crashing
  guest thread's own `%rsp` points at GUEST-mapped memory rather than its real host stack (per
  the 114th pass's own finding), so whatever native stack-walking the crash-dump path relies on
  may find a bogus/inaccessible stack and silently produce nothing for this specific fault class
  -- if true, this is a second, independent bug (a real crash producing no diagnostic at all) worth
  fixing on its own merits regardless of the STATUS_ACCESS_VIOLATION's own root cause, but NOT
  confirmed this pass (declined to chase it further this session; see "Next pickup" below). The
  burst of `[lazy_fork_commit] guard-cow: page=... INVALIDATED` lines immediately preceding each
  SIGKILL report is for a DIFFERENT address range (`0x111148000..0x111168000`) than the cdb-
  confirmed crash address (`0x7feffe490000`), so it is very likely unrelated ordinary teardown
  activity, not the crash mechanism itself -- included here only because it is the last visible
  activity before the process disappears both times, not because a causal link is established.
  **Next pickup**: (a) confirm or refute the "no dump because %rsp is a guest address" theory by
  code-reading the actual crash-dump/stack-walk implementation (search for "dumps before it
  dies"/`RECENT_FAULTS`/`RECOVERY_LOG` in `litebox_platform_windows_userland/src/lib.rs`) --  if
  confirmed, fixing the dump path itself (making it robust to a bogus `%rsp`, e.g. by capturing
  registers/a minidump via `MiniDumpWriteDump` instead of relying on stack unwinding) would give
  every future pass a working crash dump for this and any similar future fault, a durable
  improvement independent of this specific bug; (b) this `cdb`-free repro path (2/2 reproduced,
  ~7s, no RAM pressure, no debugger overhead) is a genuinely cheaper and more reliable repro loop
  than any `cdb`-based one this session found -- prefer it for future iteration once (a) gives it
  a working crash dump to read.
  - **(a) partially done, same pass, by code reading only (not yet live-tested)**: found the real,
    confirmed reason no dump appears, and it is NOT the `%rsp`-is-a-guest-address theory above --
    `write_crash_minidump` (`lib.rs:10451`) has exactly ONE call site (`lib.rs:2246`), and it is
    reached ONLY from the repeated-identical-fault circuit breaker (the same `rip` faulting more
    than `MAX_REPEATED_UNRECOV_AV = 64` times in a row on one thread, `lib.rs:2184-2252`) -- a
    genuinely UNHANDLED, ONE-SHOT access violation (exactly what every STATUS_ACCESS_VIOLATION
    this whole 114th/115th-pass investigation has captured via `cdb` looks like: it happens once
    per thread, never 64 times in a row) never reaches this call at all, regardless of AGENTS.md's
    own older, now-corrected claim that "a fatal host fault dumps before it dies, ungated." This is
    a REAL, general gap, independent of the STATUS_ACCESS_VIOLATION's own unconfirmed root cause: a
    future pass adding a minidump write to the genuine one-shot-unhandled-AV path too (the
    `[diag-unrecov-av]`-printing `else` branch at `lib.rs:2174`, reached when the host exception
    table has no covering entry for the faulting `rip`) would give every future crash of this
    shape a real dump automatically, with no `cdb` needed. **Not implemented this pass**: tracing
    exactly which branch of this ~1300-line function (`lib.rs:876` onward) OUR specific crash
    actually takes before reaching (or bypassing) that `else` branch needs more careful reading
    than this pass had time for -- the plain (non-cdb) repro's own logs show NONE of
    `[diag-unrecov-av]`/`[diag-extable]`'s output either (both described as "ungated,
    allocation-free" in their own comments), meaning our crash is intercepted even EARLIER than
    that branch, by some other part of this function's fault-classification logic, not yet
    identified. Do not add a minidump call to the `else` branch alone without first confirming
    that is actually where this crash's own dispatch goes -- it may need to go somewhere earlier.
- **116th -- the STATUS_ACCESS_VIOLATION chased since the 114th pass is ROOT-CAUSED AND FIXED
  (`702c735`); it supersedes every "mechanism unconfirmed" note above.** A lazy/guard-cow fork
  write-protects lazy-eligible groups in the parent. The exclusion for running threads' stacks was
  fed the SPAWNING thread's `rsp` (`spawn_thread`'s `ctx` still holds the caller's stack pointer;
  clone's `child_stack` is applied later, on the new thread), so a new pthread's own stack became
  `PAGE_READONLY` and its first push faulted with no usable stack. Windows cannot deliver such a
  fault to any VEH/SEH/unhandled-exception filter, which is why no handler, dump or log ever ran and
  the guest saw a bare SIGKILL. It happened BEFORE python called `fork()` (the claim was bash
  forking python; python's threads then started in memory still guarded) -- the 114th/115th
  "guard-cow claim for an earlier fork" and "which thread is it" theories were all downstream of
  this. Fix: new platform hook `note_spawned_guest_thread_stack` (`litebox/src/platform/mod.rs`,
  called from the shim's clone with the real stack top; Windows records it in
  `ALL_THREAD_STACK_RSPS`), and `classify_lazy_eligible_groups` now excludes every group overlapping
  the whole mapping containing a live `rsp`/`rsp-1`. Also added `SetUnhandledExceptionFilter`
  (`last_chance_crash_dump_filter`) so one-shot unhandled faults write a minidump (correct the older
  "dumps before it dies, ungated" claim: only the 64-repeat breaker did; undeliverable faults still
  cannot dump). **Bisect on `.wfgy/pass114_torn_read_probe.sh` (`.wfgy/pass115_plain_repro.ps1`,
  webtop:debian-xfce; no `cdb` needed)**: eager copy (`LITEBOX_PROCESS_FORK=1` only) passes end to
  end in ~10s (`PARENT_DONE`, `RC=0`, 0 torn); lazy-only gives the known guest SIGSEGV; lazy+guard-cow
  now has a correct child (0 torn). Its parent hang was mostly a LOCK-ORDER DEADLOCK I had added
  (`heal_stale_guard_entries_in_range` under `VIRTUAL_PROTECT_LOCK` in `allocate_pages` vs the guard
  VEH's registry-then-protect order; proven from a `cdb -pv` stack dump, symbolized with
  `llvm-symbolizer --relative-address`; removed, `8b982f3`; 13/20 hung before, 1/10 and 4/20 after).
  Also fixed a `FutexManager::wait` lost-wakeup window (enqueue-before-check let a wake be spent on a
  thread that returned EAGAIN; a wake racing a timeout was discarded). A RESIDUAL hang of ~5-17%
  remains on the EAGER path too (all guest threads parked in `FutexManager::wait`) -- a general
  guest lost-wakeup bug independent of fork, tracked in `.gm/prd.yml`
  (`guest-futex-lost-wakeup-residual-hang`), highest priority for app compatibility. Guard-cow itself
  remains the structural limit of user-mode COW. The plan (`.gm/prd.yml`: `native-kernel-cow-fork` and dependents; the shim already
  has a platform-neutral `has_native_fork`/`native_fork` path that Linux and macOS use) replaces it;
  that probe must print `PARENT_DONE torn=0` under the default fork with no flags. Both lazy flags
  stay default OFF. `DE_UP` still not reached; non-lazy is correctness-clean but RAM-crater-limited.
- **116th, later -- the residual multi-threaded hang is ROOT-CAUSED AND FIXED (`182819d`); this
  supersedes the "residual hang, mechanism unknown" notes above.** `WaitStateInner::wake`
  (`litebox/src/event/wait.rs`) stores the wait condition (`done`, Release) and then reads the thread
  state via `fetch_update`. When the state is not `WAITING` yet, the closure returns `None`, so NO
  locked read-modify-write runs -- only a plain load -- and x86 TSO lets that load complete before the
  preceding store is visible. The waiter meanwhile stores `WAITING` and reads `done == false`, so
  both sides miss each other and the waiter sleeps forever: a Dekker-style lost wakeup on x86
  itself (my first note that x86's locked RMW made it safe was wrong for this path). Fix:
  `fence(SeqCst)` at the top of `wake()` and `interrupt()`. Evidence on the 4-thread fork probe
  (`.wfgy/pass114_torn_read_probe.sh`): before, 13/20 then 4/20 hangs under lazy+guard-cow and
  1/20-2/12 eager; after, **50 of 50 consecutive passes** under lazy+guard-cow (0.5% chance if the
  rate were still 10%) and 7/7 eager. Method that found it: `cdb -pv` stack dumps symbolized with
  `llvm-symbolizer --relative-address`, which first exposed the `allocate_pages` lock-order deadlock
  (`8b982f3`), then the residual hang. Guard-cow is still the structural limit of user-mode COW;
  native fork remains the plan. Next: re-test the `xfce4-session` SIGABRT (needs a desktop boot,
  ~4-5 GB free), audit `ThreadHandle::interrupt` vs `prepare_to_run_guest` and `LoanList` for the
  same store-then-load pattern, then app acceptance.
- **116th, latest -- the `xfce4-session`/`ssh-agent` SIGABRT (guest pid 51) and the dbus SIGSEGVs are
  ROOT-CAUSED: lazy fork cannot work for daemonizing programs.** A lazy child fills its memory on
  demand by reading the PARENT process; a daemonizer (fork, then the parent exits) is gone before the
  child has touched its pages, so `ReadProcessMemory` fails and `lazy_commit_veh` leaves the page
  zero-filled (`lazy_fork_commit.rs`, the "unreadable in the parent" branch), and the child dies.
  Cheap repro (`.wfgy/pass116_orphan.ps1`, `debian:stable-slim`, seconds; repeated: lazy+guard-cow child dies 5/5 runs, eager child prints correctly 3/3): a subshell forks a
  background child and exits; the child reads shell variables 0.7s later. Eager fork: child prints the
  right lengths. Lazy+guard-cow: child dies with a fatal signal at 0.35s. Real boot on the fixed
  build (`.wfgy/pass116_lazy_boot1.*`, seed `pass113_de_only_ready_seed.tar`): reached Xvfb, dbus-daemon,
  xrdb, xfce4-session, xprop and `WM_POLL n=1`; the three fatal signals are dbus-daemon pids 40 and
  46 (SIGSEGV) and pid 51 (SIGABRT) whose comm is `ssh-agent` (visible now that comm is inherited; the
  113th pass mis-attributed it to xfce4-session's own fork), each dead within 0.2s of starting -- all
  daemonizers. Also in that boot: free RAM fell 5.3 -> 0.6 GB in 50s with 8 processes (killed by my
  0.7GB switch), so lazy is not delivering its RAM saving on this build -- unexplained, needs an A/B
  against the pre-702c735 binary. **Consequence: guard-cow/lazy cannot be made correct; do not
  spend more passes patching it.** The desktop path is eager fork (correct, RAM-crater-limited) until
  native kernel-COW fork (`.gm/prd.yml` `native-kernel-cow-fork`) lands. Also fixed this pass and
  worth keeping: the futex/wait StoreLoad barrier (`182819d`, 50/50 probe passes), the thread-stack
  exclusion (`702c735`), the `allocate_pages` lock-order deadlock (`8b982f3`).
Fully DONE (kept only as a marker so a future pass doesn't re-attempt): the minimal isolated
cross-process AF_UNIX repro; the `Network` shared-arena redesign's `socket_set`/
`LocalPortAllocator`/`closing_in_background`/`queued_for_closure` slice; DISPLAY/`getenv()` as the
`DE_FAILED` cause; AF_UNIX `connect()` `EAGAIN`-vs-`EINPROGRESS`; `pty_registry`/
`daemon_pty_masters` (`syscalls::pty::SharedPtyTable`, live-verified cross-process); fork's
fd-eligibility scan dropping a redirected 0/1/2 (`raw_fd_is_plain_stdio_device`);
`SharedUnixConnectQueue`'s cancel-on-first-non-blocking-miss gap (`UnixStreamState::Connecting`);
both Xvfb SIGSEGVs.

**Open, in rough priority order** (see "Where things stand" at the top of this file for the current,
authoritative detail on item 1 — this list is now just the priority index; do not treat entries here
as more current than that paragraph):

1. **Lazy-fork SIGABRT** (guest pid 51/winpid 2132, `xfce4-session`'s own fork of its next session
   client, `Signal(6)` before `execve`) — both fixed sigreturn-trampoline sub-bugs (104th/105th) are
   long closed; this is a THIRD, distinct, still-open bug, first seen in the real boot shape by the
   113th pass. See "Where things stand" for current status and the 114th pass's comm-inheritance fix
   that reopens the comm-based syscall-timeline filter as the next diagnostic tool for it. **Non-lazy
   config's blocker is purely Track B item 1's RAM crater** (below) — `xfwm4` itself launches and
   survives; no correctness bug remains on that path (109th-113th).
2. `SharedUnixConnectQueue`'s cancel-on-claim-race slot leak — FIXED 62nd. Other AF_UNIX exhaustion
   paths still silent (38th, `unix.rs`): `SharedUnixAddrPresenceTable` capacity-256 overflow; a key
   >108 bytes; backlog ignored on cross-process accept. Abstract sockets CORRECT. (The 103rd pass's
   `ECONNREFUSED`-retry-loop lead against `xfwm4` was a misdiagnosis of a false-alarm WARN, fixed and
   explained by the 109th pass — not a real blocker; don't re-open it without new evidence.)
3. `SafeZoneAllocator`'s `spin::mutex::SpinMutex` still has no dead-holder recovery — lower-urgency
   theoretical risk (the live `ssh-agent`/`xfwm4` freeze once blamed on it was actually `RawMutex`'s
   `WaiterQueue::with_lock`, CLOSED 60th/61st), not tied to any live symptom now.
4. Debugger-root-cause `litebox/src/event/wait.rs:224`'s `unreachable!()` on garbage thread state
   (dozens/boot, most frequent historical panic, NOT yet debugger-confirmed — don't patch blind).
5. `flock_registry`/`drm`/`evdev` (`GlobalState` fields) remain open, same non-POD-payload obstacle
   `SharedPtyTable` is a template for; `timerfd`/`signalfd` are the next-cheapest carriable fd kinds
   before `socket`/`unix-socket`/`epoll`; the writable-layer-visibility gap for LARGE content
   (`/tmp/de.log` etc.) needs its own chunked-publish design, not a widened `SharedFilePublishTable`
   cap. All three lower-urgency, not on the Xvfb/selkies boot path.

## 117th pass full narrative (drained from AGENTS.md by the 118th pass)

## Where things stood (117th pass)

**`DE_UP` REACHED (117th pass, first ever): eager fork + `LANG=C` + event-driven cross-process wake
(`dfdfd26`) -> `_NET_SUPPORTING_WM_CHECK` set 130 s into `.wfgy/pass117_evt_boot2` (window id
0x60008e), 20 processes, ~8 GB private, stable, HOLD loop ran on. Browser access (selkies) and app
acceptance are still untested -- that is the next pickup.** Earlier blockers below are historical. The last five blockers are understood:

- **Eager cross-process fork (`LITEBOX_PROCESS_FORK=1` alone) is correctness-clean end to end**
  (109th-113th: zero `not eligible` fallbacks, `xfce4-session` forks its clients with no crash; the
  Xvfb `Cannot open display` was a harness race, fixed by a real connect probe and `xrdb -nocpp`).
  Boot seed: `.wfgy/pass113_de_only_ready_seed.tar`. Its only blocker is RAM: every run reaches
  `WM_POLL n=3..5` (~115-130s, 15-17 concurrent processes) before free RAM crosses the safety
  kill-switch, just short of `xfwm4` setting the WM check property. Two runs from 6.1-6.4 GB free
  cratered at the same point; it needs more like 8-10 GB free sustained or a real cut in per-process
  cost (Track B item 1). **Per-process cost is not decomposed** (~350 MB-1.1 GB working set each);
  candidates: writable-layer import per child (`litebox_runner_linux_on_windows_userland/src/lib.rs`
  ~2600, copies the whole layer into each child's in-memory fs), rootfs index, guest-memory
  emulation. A 5-process `bash`+`sleep` test is only ~76 MB/process, so the big cost is specific to
  real desktop processes -- measure it (Xvfb under eager fork) before guessing. Measured 117th pass (eager boot from the ready seed,
  `.wfgy/pass117_eager_boot.ps1`, 8 GB free at start): the boot ran 152 s and hit the kill switch at 18
  processes with ~11 GB private commit (~600 MB/process; `procs=5` -> 2 GB, `12` -> 6 GB, `17` ->
  8.8 GB), reaching `WM_POLL n=5` again. `LITEBOX_DIAG_MEM_BREAKDOWN=1` (new, default off,
  `diag_private_memory_breakdown`, `[mem_breakdown]` lines, called in the fork child at each boot
  phase and in the parent at spawn) shows each child holds ~3.4 GB `MEM_MAPPED` read-only (the
  layer tars, shared page cache, NOT commit) plus ~250 MB private, of which ONE 173 MB region at
  guest address `0x7feff05a0000` is present in EVERY child right after `Platform::new()`, even
  `rm`/`mkdir`. The same image with `sleep 1 & wait` as the script (with or without the seed and
  `GLIBC_TUNABLES`) gives only 72-76 MB per child, and `debian:stable-slim` 74 MB, so the 173 MB is
  neither the rootfs index (+2 MB) nor the seed.
  **ROOT-CAUSED (117th)**: it is glibc's `/usr/lib/locale/locale-archive` (181,493,744 B = 173 MB,
  webtop image), mapped whole by `setlocale` for ANY non-`C` locale -- even `C.UTF-8`, because glibc
  looks the name up in the archive first. `de_only.sh` exports `LANG=en_US.UTF-8`; bash re-runs
  `setlocale` on that assignment. The shim's file-mapping fast path (`try_cow_mmap_file`,
  `LITEBOX_COW_MMAP`) is deliberately OFF (lossy partial-unmap zero-fill; `--oci-image` layer bytes
  are heap-owned), so the mapping is a `memcpy` into 173 MB of private commit in every process, and
  every fork child eagerly copies it again: ~430 MB/process of the ~600 MB. **Fix for the boot:
  run with `--env LANG=C`** (and `export LANG=C` in `de_only.sh`/`webtop_stack.sh`; seed
  `.wfgy/pass117_seed.tar`, driver `.wfgy/pass117_eager_boot.ps1`). Verified: root bash private
  506 MB -> 76 MB, region gone. The general defect (any large read-only private file mapping is
  committed per process and per fork) is tracked in `.gm/prd.yml` `file-backed-private-mmap-no-commit`
  and needs lossless partial-unmap recovery plus 64 KiB-aligned or platform-placed views. Full boot
  with `LANG=C` (`.wfgy/pass117_lang_boot1`): **first non-lazy boot ever to run its whole 300 s window
  with no RAM crater** -- 20 processes, ~8 GB private commit, 3.7-4.4 GB free throughout. Every
  desktop client launched (`xfwm4`, `xfsettingsd`, `xfce4-panel`, `xfdesktop`, Thunar, at-spi,
  `ssh-agent`, `gpg-agent`). `DE_UP` still NOT reached: `xfwm4` never set
  `_NET_SUPPORTING_WM_CHECK` in 300 s. Its syscall trace (`LITEBOX_DIAG_SYSCALL_TIMELINE=xfwm4`,
  `.wfgy/pass117_lang_boot2.err.log`, 121k syscalls, 4 threads) shows it ALIVE and busy, not
  hung: synchronous X request/reply loops (`writev`, `ppoll`, `recvmsg`) plus ~2,900 small-file
  opens (themes/pixmaps); it only exited (status 1) when the harness killed Xvfb at 312 s. So the
  open question is SPEED, not a deadlock: ~400 syscalls/s (with tracing) is far below native.
  Next: run untraced for >=15 min (driver's window is now 700 polls, the seed's guest poll loop
  150) with >=6 GB free, then profile where xfwm4's wall time goes (X server under litebox,
  per-syscall cost, per-request round trip). A 15-min run was cut at 130 s by host memory
  pressure (free fell from 4.4 to <0.7 GB with other host apps resident).
- **Lazy fork (`LITEBOX_LAZY_FORK_COMMIT`/`_GUARD_COW`, default OFF) cannot be made correct and is not
  a RAM win: do not patch it further.** A lazy child faults its memory in from the PARENT process; a
  daemonizer (fork, parent exits) leaves the child with zero-filled pages (`lazy_fork_commit.rs`
  unreadable-parent branch). Repro `.wfgy/pass116_orphan.ps1` (lazy dies 5/5, eager correct 3/3);
  this is the real cause of the `ssh-agent` SIGABRT (guest pid 51, comm now visible after the 114th
  comm-inheritance fix) and the dbus SIGSEGVs. Small A/B (`.wfgy/pass116_ramab.ps1`): 381 MB eager vs
  380 MB lazy, no saving. The fix is native kernel-COW fork on Windows through the shim's existing
  platform-neutral `has_native_fork`/`native_fork` path (Linux/macOS already use it) -- PRD row
  `native-kernel-cow-fork`; the spike must be written by a human (my attempt was blocked by policy).
- **Fixed in the 116th pass, all live-verified**: new-thread stack left `PAGE_READONLY` by guard-cow
  (`702c735`, root of the 114th-115th `STATUS_ACCESS_VIOLATION`/bare SIGKILL); `allocate_pages`
  lock-order deadlock (`8b982f3`); `FutexManager::wait` wasted-wake window (`8b982f3`); wait/wake
  Dekker StoreLoad lost wakeup, present on x86 too (`182819d`, probe 50/50 vs 13/20 hung before);
  last-chance minidump filter (`866ebcd`); SeqCst fences after the `WAITING`/`RUNNING_IN_GUEST`
  stores (`7b4cba9`, type-checked only; `ThreadHandle::interrupt`/`LoanList` audited, futex side
  already ordered).
- **Environment limit, 2026-09-28**: this host (15.6 GB) had 0.3-1.7 GB free with Chrome/Discord
  resident; no desktop boot or release build was possible. A boot needs ~5 GB free for its whole
  length; never close the user's applications to get it.

- **Cross-process unix-socket wake is now event-driven (117th, `dfdfd26`)**: it used to be NO wake at
  all -- every blocked read/poll on a shared connection waited out `SHARED_UNIX_POLL_INTERVAL`
  (15 ms), so an X11 round trip cost ~15-30 ms and `xfwm4`'s ~7,000 round trips alone took minutes.
  A send/recv/close on a shared slot now sets the peer host's existing wake event
  (`wake_signal_listener`, handle-cached), the listener (`drain_host`, `xproc.rs`) bumps
  `litebox::event::polling::bump_external_wake_epoch` (a `wait_on_events` waiter treats a changed
  epoch as "re-run `try_op`") and wakes waiting threads (`ThreadHandle::wake_if_waiting`; poll/ppoll
  re-scan on any wake). Measured `.wfgy/pass117_pingpong.sh`-style socketpair ping-pong across a
  cross-process fork: 19.9 ms -> 0.05 ms per round trip. Lesson: a bare `Waker::wake` does NOT
  make a `wait_on_events` waiter retry (its ready-check is the observer flag) -- it needs the epoch.
  Still to do: rendezvous (`accept`/`connect`) and pty waits still poll; cross-process PIPES hang
  when the parent keeps both ends open (PRD `cross-process-pipes-not-shared`); a
  `connect()`/`accept()`-made local pair carried over fork gives EPIPE (PRD
  `shared-unix-bound-socket-fork-broken-pipe`). Full boot with this build: not yet run.

- **Browser path reached (117th, later)**: real Chrome (gm `cdp`, session `p117-browser`) loads selkies'
  own web UI from the guest over `--publish 8081:8081` and receives live video ("Stream started", first
  stripe decoded ~18 s after connect; `.gm/witness/p117_stream_try5.png`, black because that probe ran
  Xvfb only). Needed, all committed: socket buffers in shared-arena pools (`a596c03`), only the
  gateway-owning process polls the interface (`92a2d0e`), `/etc/hosts` + `IPV6_V6ONLY` (`8d13f7d`),
  `FIONCLEX` (`d9027c5`, `os.set_inheritable(True)` used to fail EINVAL and break every uvloop
  subprocess incl. selkies' `pgrep`), registry timeouts/retries + cached layer list (`e35a36a`+next).
  Harness facts (all in the untracked `.wfgy/`): selkies 2.0 needs `--enable-basic-auth=false` and
  `--addr=0.0.0.0` (not `localhost`); pass `--env LC_ALL=C` (Python otherwise maps the 173 MB
  locale archive: selkies 1,211 -> 626 MB); a child process's output only reaches a redirected stdout
  when the child writes the inherited handle directly (no pipe): pipes deliver at writer EXIT
  (PRD `cross-process-pipe-streaming-blocked-by-sibling-bridge`); use `cmd /c "runner ... < in > out"`
  drivers, and `exec selkies` as the main guest process to read its log.
  **Full stack (Xvfb+selkies+DE+nginx) still cannot finish under ~7 GB free**: ~600 MB Xvfb (117 MB
  memcpy'd library mapping + heap arenas), ~626 MB selkies, ~100 MB per other process. The general
  fix is file-backed mappings that are shared, not copied (PRD `file-backed-private-mmap-no-commit`).
  `LITEBOX_DIAG_MEM_BREAKDOWN=1` + `LITEBOX_DIAG_MEM_BREAKDOWN_LATE=<secs>` prints each process's
  regions at start and N s later.

**Next pickup, in order**: (1) with >=8 GB free run `de_only.sh` from the ready seed under eager fork
and reach `DE_UP`; (2) decompose per-process memory (sample `PrivateMemorySize64` per runner against
a `VirtualQuery` breakdown) and cut the biggest piece; (3) app acceptance from a real browser
(terminal, Thunar, Mousepad, settings, panel, Ristretto, a web browser; second client and reconnect)
on Windows, then Linux/macOS builds; (4) native kernel-COW fork, then retire `lazy_fork_commit.rs`.
The PRD (`.gm/prd.yml`, gitignored) carries the full task list.

## 118th pass -- session-client death cascade, full iterative narrative

**Bigger, better-evidenced finding this pass, NOT yet root-caused: `xfce4-session` self-terminates
minutes into a stable run, cascading to kill every client it started.** The original driver
(`de_only.sh`) trims the Failsafe session to just `xfwm4`+`xfsettingsd` (82nd-pass "Angle B", real
and intentional for that narrower investigation) -- removing that trim in `pass118_full.sh` (no
`xfce4-session.xml` override) makes the REAL 5-client Failsafe session run, and **all five clients
launch successfully**: `xfwm4`, `xfsettingsd`, `xfce4-panel` (with two `wrapper-2.0` plugin hosts),
`Thunar` (`thunar-real`), `xfdesktop` -- confirmed via `DIAG_TIMELINE execve` for every one of them
(`.wfgy/pass118_full7.err.log`, `LITEBOX_DIAG_SYSCALL_TIMELINE=xfce4-session`). But `xfce4-session`
itself (pid 28 in that run) later calls a plain `exit_group(status=1)` right after an ordinary
`recvmsg` on its own fd 3 returns successfully -- no crash, no signal, no error logged anywhere in
its own trace immediately before. **Root mechanism narrowed further** (`.wfgy/pass118_full8.*`,
`LITEBOX_DIAG_SYSCALL_TIMELINE=xfce4-session,xfwm4,xfsettingsd,xfce4-panel,Thunar,thunar-real,
xfdesktop,wrapper-2.0,dbus-daemon` -- traces every session client, not just the manager): the deaths
are NOT simultaneous, they are a STAGGERED CASCADE, and `xfce4-session` itself dies LAST, not first --
sorting every traced `exit_group` by its own numeric timestamp gives a clean, consistent order:
`xfdesktop`(t=581s) -> `thunar-real`(592s) -> `xfce4-panel`(604s) -> `xfsettingsd`(613s) ->
`xfwm4`(626s) -> `xfce4-session`(684s), each roughly 10-45s after the previous. Every single one of
these six deaths shares the IDENTICAL immediate shape: an ordinary `recvmsg(sockfd=3, ...)` that
returns successfully (`ok=true`), immediately followed by `exit_group(status=1)` -- no error, no
signal, nothing else in that thread's own trace between the two. Traced `fd 3`'s origin for
`xfdesktop` specifically: `socket(AF_UNIX)` + `connect()` (first attempt `addrlen=25` fails, second
`addrlen=20` succeeds) right after its dynamic-linker phase -- this is each client's own X11 display
connection, opened once at GTK/Xlib startup and read from for its whole life via `recvmsg`, not a
per-request socket. **The shape (a normal-looking read that returns OK immediately followed by a
clean `exit(1)`) matches Xlib's own default `_XIOError` handler** ("X connection ... broken", called
when the X connection unexpectedly delivers EOF or a protocol violation) far better than a crash or
an explicit kill -- if so, the real question is why each client's X11 connection independently goes
bad, staggered over ~100s, roughly (but not exactly -- `xfce4-panel` before `xfsettingsd`, not launch
order) in reverse-priority order. Timing is not fixed across runs (~170s/~285s/~470-490s/~581-684s
seen); **ruled out**: the harness's own periodic `xprop -root` polling (removed entirely in
`.wfgy/pass118_noxprop.sh` -- still died, just later), RAM pressure (rock-stable 4.4-4.5GB free
through one death with zero dip), and `SharedUnixAddrPresenceTable` exhaustion (`unix.rs:2988`,
`UNIX_ADDR_PRESENCE_CAPACITY = 256` -- only 68 traced `connect()`/73 `socket()` calls total across
the whole run for these 9 comms, nowhere near 256, and that table indexes bound/listening addresses
per RFC, not per-client connections, so ordinary GUI clients barely touch it). **One still-open,
unconfirmed lead from an earlier (xprop-polling) run**: a SECOND `xfwm4` instance appeared
~40-70s before ITS death (`xfwm4-WARNING: Another compositing manager is running on screen 0`, a
distinct pid) -- i.e. `xfce4-session` had already respawned a client that died earlier still, meaning
the visible cascade order above may itself be downstream of an even earlier, unlogged first death.
Every guest app IS reachable and does launch given the real (untrimmed) session config -- the
"apps must work" gap is entirely this later self-termination cascade, not a launch failure.
**The death order is deterministic, not racy, and reproduces across every run** (3/3 traced runs,
`.wfgy/pass118_full{8,9}.*`): always `xfdesktop` -> `thunar-real` -> `xfce4-panel` -> `xfsettingsd`
-> `xfwm4` -> `xfce4-session` -- the EXACT REVERSE of their launch order (xfwm4=Client0 launches
first, xfdesktop=Client4 launches last). `xfce4-session` itself is confirmed to send NO explicit
`kill`/`tgkill`/`tkill` syscall to any of them (grepped its whole traced syscall stream, zero hits)
-- so this is not xfce4-session deliberately terminating its own session in reverse-priority order;
each client is independently reaching the same fate on its own. Combined with the reverse-launch
ordering, this fits "each client independently dies after being idle/alive for very close to the
SAME duration since ITS OWN startup" (they all launch within ~0.3s of each other in real time, so
comparing their own per-process-relative elapsed-time clocks -- which each reset to ~0 at that
client's own start, the standing `init_logging()` caveat -- is valid to within that same ~0.3s
slop). But the actual duration is NOT a fixed constant: 3 traced runs died at respectively ~170s,
~580-684s, and ~773-882s since session start, more than a 4x spread, so if there IS a shared
per-client "idle timeout" mechanism, its effective duration is load/real-time dependent, not a
compiled-in constant -- consistent with a litebox scheduling/timing artifact (e.g. a guest
timerfd/nanosleep-based watchdog whose real wall-clock firing time depends on host CPU contention)
more than a real GLib/Xlib application-level timeout, which would fire far more consistently.
**RESOLVED to a genuine EOF on the X11 socket** (`.wfgy/pass118_full10.*`): `litebox_diag::socket_read`
did not cover `recvmsg` at all (only `read`/`readv`, `syscalls/file.rs`) -- FIXED this pass
(`25c2453`, `litebox_shim_linux/src/syscalls/net.rs`'s `do_recvmsg`, mirrors the existing
`file.rs` hook exactly), and with it working, `xfdesktop`'s (pid 156, this run) very last `recvmsg`
on its X connection (fd 3) is `size=0 preview="[]"` -- a real, clean EOF -- immediately before its
`exit_group(1)`. This is EXACTLY Xlib's own default `_XIOError` behavior ("X connection ... broken")
firing on unexpected connection loss, not a corrupted/truncated message or a guest protocol bug.
Every OTHER `recvmsg` in the preceding ~750s is a size=32, byte-identical payload
(`96 00 cf 02 03 00 80 00 03 00 80 00 ...` -- `0x96 & 0x7f = 0x16 = 22` decimal = X11 core event
code `PropertyNotify`) arriving at an exact, fixed 60-second period -- a real but UNRELATED periodic
event (very likely a clock/taskbar-widget touching a root-window property once a minute), ruled out
as the trigger since the final EOF lands ~35s after the last one of these, not on its own 60s
boundary. **So the open question is now precisely**: why does Xvfb (or litebox's own AF_UNIX
relay/connection-carrying layer) close THIS client's connection. Xvfb's own guest stdout has zero
disconnect/error/client-related output at any point in the run, and a broad keyword search
(`shutdown`, `ESHUTDOWN`, `queued_for_closure`, `SharedUnixConnectQueue`, `dead.?owner`, `reclaim`)
across the WHOLE traced log turned up nothing -- the teardown is genuinely silent in the current
code, meaning it is either an intentional-but-unlogged path or a real bug with no diagnostic
covering it yet. Added logging (`10b2636`/`ba4ff9d`, `litebox_diag::unix_conn_teardown`, `unix.rs`) at
BOTH real teardown call sites -- `SharedView::release_holder` (a holding process's fd/table drop)
and `SharedView::shutdown_write` (an explicit `shutdown(fd, SHUT_WR)`) -- and re-ran the full
capture **three more times** (`.wfgy/pass118_full{11,12,13}.*`; the 12th ran the full 900s with NO
death at all, confirming the death is genuinely intermittent/load-dependent, not a guaranteed
per-boot event). **Neither hook fired at or before `xfdesktop`'s own death time in any of the three
captures** -- the only `unix_conn_teardown` lines near each death are `xfdesktop`'s OWN process
exiting a fraction of a second later and releasing ITS OWN held slots as ordinary cleanup, not
something happening TO it beforehand. **Real remaining lead, from reading `recv()`'s own body**
(`unix.rs`, `SharedView::recv`): the EOF is synthesized by `peer_gone()`, which checks
`slot_ref.side_gone(!self.is_client, ...)` FRESH on every call, not by any event delivered at read
time -- so the ACTUAL moment the peer side "went away" could have happened much EARLIER in the
run and simply gone unnoticed (and unlogged by anything gated on the read/recv path) until this
read finally happens to check again. Neither of the two teardown hooks would necessarily fire
"close to" the observed death time at all if this is what's happening. **Next session, concretely**:
(1) capture the SLOT NUMBER alongside the `recvmsg` payload trace (net.rs's `do_recvmsg` diagnostic
does not currently have access to the underlying `UnixSocket`'s slot -- needs plumbing through, or
a lower-level hook directly in `SharedView::recv`/`peer_gone` instead of `net.rs`) so a capture can
directly correlate "this fd's connection is slot N" against "slot N's side went away at time T",
however much earlier T is; (2) with that correlation, check whether the responsible slot's `side_gone`
transition traces back to Xvfb's own process, or to some OTHER, unexpected host process ever having
briefly held (and released) that slot -- a bug in how connections get attributed to holders would
explain an early, silent, unnoticed release far better than anything actually wrong with Xvfb
itself, which by every account (its own log, the periodic legitimate `PropertyNotify` traffic
still flowing right up to 35s before the end) stays healthy and correct throughout.
