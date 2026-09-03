# litebox — current state (2026-09-03)

This file is the authoritative, up-to-date picture of what works, what's broken, and what to do
next. It replaces the previous pass-by-pass chronological log, which accumulated thousands of
lines of retracted hypotheses alongside real findings. The full prior history (every pass,
including every dead end) is preserved at `docs/AGENTS_ARCHIVE_2026-09-03.md` for anyone who
needs the detailed forensic trail — but start here, not there.

## Standing goal

Get XFCE actually rendering and staying up under litebox on a Windows host (no WSL, no
hypervisor — see `feedback_no_wsl_or_hypervisor` in project memory).

**MET.** A full XFCE desktop renders and stays up to the end of a run, verified against the
standing success oracle (`non_black_pixels > 0` in the FINAL frames, not just any frame
mid-run): 24 frames captured, EVERY ONE non-black, final five all at `non_black_pixels=2,073,597`,
no zero frame anywhere in the run, `TEST_DONE` reached. All six components alive at the end
(t=74.1s): `weston`, `xfconfd`, `xfwm4`, `xfsettingsd`, `xfdesktop`, `xfce4-panel` — zero exit with
a failure status. `DBUS_UP=yes`, `XFCONF_PROBE_RC=0`, `XFCE_DISPLAY=:0`. The only remaining
messages in any component's stderr are non-fatal warnings (AT-SPI accessibility bus absent —
optional, no accessibility daemon in this layer; upower proxy refused — no power daemon in the
layer; `SESSION_MANAGER` unset — expected, this launcher deliberately bypasses `xfce4-session`).
Working launcher: `advisor/probes/run_xfce_xwm.sh`, committed `4e6fc556`.

**Root cause of the entire session-long blocker, and the fix — both non-litebox, zero litebox
code changes required:**
1. **Missing XWM.** Launch scripts spawned `Xwayland` as a bare separate process. Rootful Xwayland
   needs the launching compositor to attach an X Window Manager over a `-wm <fd>` connection —
   that's what maps an X11 window's surface into the compositor's scene graph. weston's
   `desktop-shell.so` has no XWM logic of its own; that lives exclusively in weston's own
   `xwayland` module, loaded via `[core] xwayland=true` in `weston.ini`, which spawns AND manages
   Xwayland itself (including the `-wm` handshake). Without it, X11 client surfaces got real pixel
   content written into their buffers (independently verified byte-identical via same-instant
   cross-process comparison — litebox's shared-memory path was never at fault) but were never
   mapped into weston's scene graph, so nothing ever composited — the "renders fine, then goes
   black and never recovers" symptom that dominated this entire session.
2. **`set -x` in the launch script.** Shell tracing deterministically triggers a real, separate,
   still-open litebox bug (a trampoline `#UD` at `rip=0x7feffff7fb8a`, deterministic 30s repro at
   `advisor/probes/setx_ud_repro.sh`) that kills the first backgrounded child before it reaches
   `execve()` — this is what was taking out `dbus-daemon` specifically, cascading into
   `xfconfd`/`xfsettingsd`/`xfce4-panel` all failing with "Connection refused". `set -x` was
   reintroduced by copying an older script mid-session and cost real additional time before being
   caught a second time — treat as a standing hazard, not a one-off.

**Durable launch-script configuration (6 items — apply to every XFCE launch script, not just the
one already fixed)**:
1. `weston.ini`: `[core] xwayland=true`.
2. No manual `Xwayland` launch — let weston manage it.
3. Discover the display weston chooses (currently `:0`) rather than hardcoding `:1`.
4. **No `set -x` anywhere** in the script or anything it sources — use explicit `echo` markers at
   stage boundaries instead. Grep for this explicitly when touching any launch script; it is easy
   to reintroduce by copying.
5. Single dbus spawn, no retry — retrying a backgrounded spawn after losing one child to the `#UD`
   kills the launcher shell itself, not just the child. If dbus is lost, rerun the whole script.
6. Capture backgrounded services' stderr AND print/tee it, so a fast fatal crash never presents as
   a silent, misleading readiness-timeout.

**What's still open, but no longer blocking**: the trampoline `#UD` itself (root-caused to
`set -x`, but the underlying litebox bug that fires ANY time a backgrounded child races that
specific instruction sequence is real and unfixed — just no longer triggered now that `set -x` is
banned from launch scripts). Worth closing eventually per the standing "always build/fix, don't
just work around" discipline, but does not block the standing goal, which is met. See
`advisor/probes/setx_ud_repro.sh` for the repro.

See "Rendering/scanout blocker" below for the full forensic trail (kept for anyone who needs the
detailed history of how this was diagnosed — memory-corruption theories all refuted, compositing
theory confirmed via same-instant cross-process comparison, root cause found via targeted web
research on weston/Xwayland internals).

## Standing directives (do not relitigate these)

- **Never conserve token/context budget.** No instruction anywhere says to throttle effort or
  stop at a token threshold. Work at full effort until the actual goal is met. If genuinely near
  a context ceiling, let auto-summarization handle it or checkpoint via a commit — never
  preemptively cut work short and frame it as budget conservation.
- **Always build debug/observability tooling proactively while investigating**, not just enough
  to explain the current bug. Several real bugs this session were only found because someone
  built a general tool (dependency auditor, preflight checker, per-process stderr capture)
  instead of continuing to guess from interleaved logs.
- **Use `/gm` to drive non-trivial coding tasks.** Zero branches or worktrees — work on `main`.
  Commit only as `lanmower`, never attribute anything to Claude/AI.
- **The success oracle for a launch run is `non_black_pixels > 0` in the FINAL frames of a run**
  (via `LITEBOX_DUMP_FRAMES=1`), not "any frame anywhere in the run." A run can render a correct
  desktop for 20+ seconds and then go black — that is not success. This distinction cost real
  time this session (a claimed "XFCE working, sustained 30+ seconds" turned out to be weston's
  own render, with the screen already black by the time XFCE's own components started).
- **Exit code is not a valid success oracle either.** A run can exit 0 while several forked
  children were silently killed by fatal signals. Count "fatal signal" lines in
  `LITEBOX_LOG=error` output directly.

## What's confirmed working (do not re-investigate)

- **litebox's DRM/display emulation is sound.** Mode enumeration, dumb-buffer allocation, the
  pixman renderer, and swapchain creation all work correctly against the real virtual DRM output
  ("Virtual-1"). Verified via targeted DRM-ioctl logging and direct frame inspection.
- **weston renders a complete desktop reproducibly** under litebox (`--use-pixman
  --shell=desktop-shell.so`), independently confirmed across many runs this session
  (`non_black_pixels=2073597`, the canonical full-1920x1080-desktop signature).
- **labwc is NOT the right compositor to keep pursuing.** It unconditionally creates a transient
  0x0-dimension headless output on startup as a documented upstream workaround (`src/server.c`,
  `wlr_headless_add_output(backend, 0, 0)` immediately destroyed) — normal on real hardware where
  the create-destroy completes before anything renders to it. Under litebox's timing, something
  renders/modesets it inside that window, hitting wlroots' `assert(width > 0 && height > 0)` in
  `wlr_swapchain_create` and aborting. This is real upstream behavior interacting with litebox's
  timing, not a litebox display bug — use weston instead; it's already proven working.
- **Windows fork-emulation machinery (`fork_verify.rs`) is Windows-only by architecture.** Real
  Linux/macOS `fork()` gives the child identical virtual addresses for free and never needs this
  machinery — never assume a `fork_verify`-attributed crash needs investigating on those
  platforms, and never port a `fork_verify.rs` fix there.

## Real bugs found and fixed this session (all committed on `main`)

1. **Layer packaging: `tar` drops SONAME symlinks.** Extracted layer tars kept only the versioned
   filename (`libX11.so.6.4.0`) for hundreds of shared libraries, never the plain SONAME
   (`libX11.so.6`) the dynamic loader actually looks up — because `tar` doesn't reliably recreate
   a symlink before its target exists on extraction. Silently broke `xfwm4`, `xfdesktop`,
   `xfce4-panel`, and hundreds of other binaries (946 of 1696 ELFs in one measured layer). New
   tools: `advisor/probes/audit_layer_deps.py <extracted-root>` (scans, reports every unresolved
   `DT_NEEDED`, zero exit code = clean) and `advisor/probes/fix_layer_sonames.py <extracted-root>`
   (creates the missing links from each `.so`'s real `DT_SONAME`, idempotent, `--dry-run`
   available). Also `advisor/probes/preflight_layer.sh` — a fast, fail-loud check of a layer
   before spending a full run on it (shell/loader present, `ld-musl` search path present, zero
   unresolved deps, at least one X client, session XML present).
2. **Host-crash race: `unmap_shared_memory` unsynchronized against `update_permissions`.**
   `litebox_platform_windows_userland/src/lib.rs`'s `unmap_shared_memory` (`UnmapViewOfFileEx`)
   was the one Windows VAD-tree mutator in the file not serialized under `VIRTUAL_PROTECT_LOCK`,
   unlike every other allocate/deallocate/protect path. A guest process's real `munmap()`/exit
   teardown of a shared mapping could race a different thread's concurrent `mprotect()` on the
   same region: the region gets freed between `update_permissions`'s `MEM_COMMIT` query and its
   actual `VirtualProtect` call, which then observes `MEM_FREE`, fails with a spurious
   `ERROR_SUCCESS`, and trips an `assert!` that panics the whole host process. Fixed by adding the
   missing lock guard (18 lines). Verified: previously guaranteed a host panic within ~46s of an
   XFCE launch; post-fix, a full 100s run produced zero panics. Commit `984927b0`.
3. **`sys_mprotect` bitmask gap, `remove_mapping` clamp scoping, `VmArea`/`rangemap` coalescing
   bug, `deallocate_pages` missing MEM_MAPPED guard, `memfd` mmap-time wipe bug.** Multiple real
   memory-management correctness fixes landed earlier this session — see git log
   (`3a0755e4`, `140c1711`) for full detail. None were the cause of the crashes described below,
   but all are real, verified fixes worth keeping.
4. **`protect_mapping` caller attribution.** Every call site of `Vmem::protect_mapping` now tags
   its caller (`guest_mprotect` / `make_pages_*` / `create_mapping` / `fork_duplicate`), so a
   future protection-related crash can be attributed to its actual origin in one log read instead
   of hours of inference. This tooling investment paid for itself directly this session.
5. **Unlocked-write gap in proactive fork stale-pointer fixup.** The proactive
   `fixup_stale_stack_pointers`/`fixup_stale_elf_data_pointers` pass (runs on the parent's own
   thread right after `PageManager::duplicate()`) rewrote a freshly-forked child's memory with no
   locking at all — not against `fork_verify`'s existing reactive healing lock, nor against a
   second concurrently-forking parent thread's own proactive pass. Fixed by adding
   `ForkChildVerificationProvider::lock_fork_verify_heal()` and wrapping both calls with it.
   Commit `bcc6a3e7`. **Real and worth keeping, but does NOT by itself fix the open blocker
   below** — direct measurement showed no change in fault rate; see "Open blockers" for the
   still-open mechanism.

## Open blockers (the real remaining gap)

**MAJOR UPDATE (this session, latest): a complete XFCE desktop now comes up and stays alive.**
weston, Xwayland, xfconfd, xfwm4, xfsettingsd, xfdesktop, and xfce4-panel all start successfully
and remain alive to the end of a run (confirmed: `DBUS_UP=yes`, `XFCONF_PROBE_RC=0`, no component
exits with a failure status, only cosmetic warnings in their stderr — AT-SPI accessibility bus
address errors, missing GSettings schema, no SESSION_MANAGER var, none fatal). This required BOTH
of the concurrent-fork fixes below AND removing `set -x` from the launch script (see the #UD
bisection further down — `set -x` itself was triggering a real, separate litebox bug that broke
the launch chain). **The remaining gap is now narrow and purely a rendering/scanout issue, not a
process-launch issue**: weston renders a full desktop correctly at t=7.4s
(`non_black_pixels=2073597`), then the framebuffer goes black at t=20.3s and never recovers —
this happens the moment Xwayland forks `xkbcomp` after a large pointer-healing pass, BEFORE any
XFCE component even starts (all XFCE components start from t=29.9s onward, well after the
blackout — neither XFCE nor `xfwm4` causes it). See "Rendering/scanout blocker" below for the
precise next diagnostic. **Two process-level bugs are fully fixed** (see "FIX LANDED" further
down for both); **one process-level bug remains open but no longer blocks the launch chain**
(the `set -x`-triggered #UD — has a fast deterministic repro, still needs a real fix, see below).

**Historical framing (both now fixed, kept for context): forked children died (SIGSEGV/SIGILL,
`rip==cr2`) before they could `execve()`, under concurrent forking only.** The earlier
"MAXCONCURRENT fork_verify healing passes" theory below is REFUTED as of a later measurement —
read the correction further down before acting on it.

- Reproduces on a **bare alpine rootfs with zero display components** — no weston/Xwayland/XFCE
  needed. A background/concurrent-fork shell pattern alone triggers it. Sequential forking is
  rock-solid (0 faults across repeated runs); only concurrent forking triggers it.
- Concretely, this kills `dbus-daemon`'s forked child before it reaches `execve()` in a typical
  XFCE launch, which cascades: no D-Bus session bus → `xfconfd`/`xfsettingsd`/`xfce4-panel` all
  fail with "Connection refused" → `xfce4-session` launches zero children. This is why the
  desktop currently goes black by the end of a run even with the DRM/weston/host-panic layers all
  working correctly — everything downstream of dbus never gets its settings/session
  infrastructure.
- Regression oracle (sub-second to a few seconds, litebox host + bare rootfs, no display stack
  needed):
  ```
  FAIL case (must go to 0 faults after a real fix):
    i=1; while [ $i -le 30 ]; do /bin/true & i=$((i+1)); done; sleep 2
    (currently: 3-6 "fatal signal" log lines per run, unchanged by the fix below)

  PASS control (must STAY at 0 — don't break this while fixing the above):
    i=1; while [ $i -le 10 ]; do sleep 5 & sleep 0.3; i=$((i+1)); done; sleep 6
    (was 0,0,0; noted as occasionally noisy on a loaded host in the most recent session — treat
    a single nonzero reading here with suspicion and rerun before trusting it as signal)
  ```
- **A real, genuine locking gap WAS found and fixed** (commit `bcc6a3e7`): the proactive
  `fixup_stale_stack_pointers`/`fixup_stale_elf_data_pointers` pass
  (`litebox_shim_linux/src/syscalls/process.rs`'s `do_clone`, runs on the parent's own thread
  right after `PageManager::duplicate()`) rewrote a freshly-forked child's memory with **zero
  locking** — not serialized against `fork_verify`'s reactive AV-path/single-step healing lock
  (`FORK_VERIFY_HEAL_LOCK`, which already existed but was never wired into this path), nor
  against a second concurrently-forking parent thread's own proactive pass. Fixed by adding
  `ForkChildVerificationProvider::lock_fork_verify_heal()` and wrapping both proactive fixup
  calls with it. This is a correct, worthwhile fix on its own merits — but **direct measurement
  shows it does NOT reduce the fault rate of the bug described here.** Landed and kept regardless.
- **CORRECTION — the "MAXCONCURRENT fork_verify healing passes" correlation's CAUSAL EXPLANATION
  was wrong; the correlation itself was real.** Setting `LITEBOX_FORKVERIFY_OFF=1` (disables
  fork_verify's reactive single-step/AV-path healing entirely, proactive fixup left on)
  reproduces the SAME fault rate as normal — proving reactive healing machinery is NOT itself the
  mechanism. But the underlying 41-run measurement (0 faults in 8/8 at MAXCONCURRENT==1) was not
  spurious: concurrent fork_verify healing passes were a PROXY for concurrent
  `PageManager::duplicate()` calls — the actual root cause, fixed below (`166b5a90`) — since both
  scale together with concurrent forking. Record this as "correct correlation, wrong causal
  attribution," not "the correlation was noise": it remains a useful detector for this class of
  concurrency bug even though the fix landed elsewhere. Conversely, disabling the *proactive*
  fixup pass instead spikes faults to ~31/run (nearly every child) — confirming that pass does
  real, necessary work and is not itself spurious corruption.
- **Crash signature re-examined and clarified**: `rip==cr2`, offset `0x1464b`/`0x464b` low bits,
  confirmed via `objdump` disassembly to be a **real, valid busybox instruction**
  (`lea 0x148(%rbx),%rax`) at the CORRECT offset relative to the child's own load base — this is
  not a corrupted/wrong jump target. Windows is genuinely reporting that page as not-present at
  the moment of the fault. No address-range collisions were found across extensive
  `fork_duplicate`/`create_mapping`/`guest_mprotect` log cross-referencing between concurrently
  forking children.
- **`VirtualQuery`-at-fault-time measurement DONE this session — corrects the "not-present"
  characterization above.** Added `LITEBOX_DIAG_FAULT_VQ=1` to `vectored_exception_handler` in
  `litebox_platform_windows_userland/src/lib.rs` (gated on `rip == cr2`, the documented crash
  signature, to avoid the 268,000+ line flood an ungated version produces from `fork_verify`'s own
  expected healing faults — confirmed live). Real captures from the regression oracle (3 real
  crashes in one run, all 3 captured cleanly):
  ```
  cr2=rip=0x153e464b  state=0x1000 (MEM_COMMIT)  type=0x20000 (MEM_PRIVATE)  protect=0x2 (PAGE_READONLY)
  cr2=rip=0x1fb4464b  state=0x1000 (MEM_COMMIT)  type=0x20000 (MEM_PRIVATE)  protect=0x2 (PAGE_READONLY)
  cr2=rip=0x3b13464b  state=0x1000 (MEM_COMMIT)  type=0x20000 (MEM_PRIVATE)  protect=0x2 (PAGE_READONLY)
  ```
  All three: the page IS committed and present (contradicting the earlier "genuinely not-present"
  read) — it is **`PAGE_READONLY` where `PAGE_EXECUTE_READ` is expected** for a code page about to
  execute an instruction-fetch. This is a real permissions bug, not a missing-mapping bug. Given
  `prot_flags()` (same file) correctly maps `VmFlags::VM_EXEC` to `PAGE_EXECUTE_READ` and
  `Vmem::duplicate`'s eager-copy path (`litebox/src/mm/linux.rs`) correctly calls
  `protect_mapping(dest_range, vma.flags.into(), "fork_duplicate")` with the source region's real
  flags, the most likely explanation is that the exec-narrowing `protect_mapping` call for this
  specific region either never ran, or ran and was then overwritten back to `PAGE_READONLY` by a
  DIFFERENT thread's own operation on the same or an adjacent real address before the child ever
  got to execute there. Not yet root-caused to an exact line.
- **Tried and REVERTED**: wrapping the entire `PageManager::duplicate()` call in `do_clone`
  (`litebox_shim_linux/src/syscalls/process.rs`) with `lock_fork_verify_heal()` (the same guard
  already used for the proactive stale-pointer fixup passes) was a natural next attempt given the
  above evidence, but **measured to make the oracle worse**, not better (30-concurrent-`/bin/true`:
  baseline 3-6 faults rose to 9-11 across 5 reruns with this lock held). Reverted; a comment is
  left at the call site so this specific change is not retried without new evidence. The real fix
  needs to narrow down WHERE inside `duplicate()`'s per-region loop the wrong protection value
  reaches `update_permissions`/`VirtualProtect`, not just serialize the whole call more broadly.
- **Host resource exhaustion recurred this session**, matching a pattern this project's own memory
  already documents (`pass 314`/`pass 315`'s "Heisenbug-shaped timing race" vs. genuine host
  degradation distinction): free physical memory dropped to ~1.1-1.3 GiB out of 16 GiB after
  several build+run cycles and did not recover after several seconds idle, with fault counts on
  BOTH the FAIL oracle (rising to 8-12) and the PASS control (rising to 5-8, which must stay 0)
  becoming unreliable at the same time. Every number from this investigation from that point
  forward should be treated as suspect until re-measured on a fresh host/session state — this is
  the same known confound, not new evidence of anything code-related.
- **Next concrete step**: re-run the `LITEBOX_DIAG_FAULT_VQ=1` capture on a FRESH host session
  (low memory pressure) across several of the 3-6 baseline faults, this time also logging, from
  `Vmem::duplicate`'s own per-region loop, the exact `(source_range, dest_range, vma.flags)` for
  every region as it is placed — then cross-reference by dest-address against the `cr2` values
  captured here to identify definitively whether the faulting address's OWN `protect_mapping` call
  ran at all, and with what flags, versus was silently skipped or overwritten afterward.

- **FIX LANDED (this session): root mechanism found and fixed, regression oracle verified clean.**
  Confirmed by direct code reading: `PageManager::duplicate()` (`litebox/src/mm/linux.rs`) runs
  entirely on the PARENT's own thread — the new child's real OS thread does not exist yet
  (`spawn_thread`/`std::thread::Builder::new()` in `litebox_platform_windows_userland/src/lib.rs`
  runs strictly AFTER `duplicate()` returns, see `do_clone` in `litebox_shim_linux/src/syscalls/
  process.rs`). Every `allocate_pages`/`protect_mapping` call `duplicate()` makes therefore runs
  under `current_claim_owner()` == the PARENT's own `ClaimOwner` (`CURRENT_GUEST_PID` is only
  repointed at the CHILD's pid later, inside the new OS thread's own closure, via
  `reclaim_ranges_for_fork_child` — long after `duplicate()` already ran). Windows' own
  `CLAIMED_RANGES` foreign-claim defense (`claim_range`, `find_foreign_claim`, same file) exists
  specifically to stop one guest process's allocation from silently decommitting/recommitting over
  a DIFFERENT, still-live guest process's memory — but it only recognizes a claim as foreign when
  its owner differs. Two SIBLINGS forking CONCURRENTLY from the same parent both get their entire
  eager address-space copy attributed to that SAME parent owner, so `claim_range`'s own
  same-owner-coalescing fast path (deletes and merges any prior claim from the "same" owner that
  overlaps or touches the new one — by design, this is what keeps ordinary sequential heap growth
  on one thread cheap) can merge/absorb one sibling's just-placed destination region into the
  other sibling's own claim, making the `Replace`-mode per-region placement blind to a genuine
  cross-sibling collision it would otherwise have caught and relocated away from. This produces
  exactly the observed `PAGE_READONLY`-where-`PAGE_EXECUTE_READ`-expected signature: one child's
  freshly-narrowed R+X code page gets silently decommitted/recommitted (back to the eager-copy's
  initial R+W, before ITS OWN later narrowing step runs) by a concurrently-copying sibling that
  was never flagged as foreign.

  **Fix**: added `ThreadProvider::with_fork_duplicate_claim_owner(child_pid, f)` (default no-op,
  `litebox/src/platform/mod.rs`), implemented on Windows (`litebox_platform_windows_userland/src/
  lib.rs`) as a save/restore of the CALLING (parent) thread's own `CURRENT_GUEST_PID`
  thread-local around `f`. `do_clone` (`litebox_shim_linux/src/syscalls/process.rs`) now wraps
  the `PageManager::duplicate()` call with `self.global.platform.with_fork_duplicate_claim_owner
  (child_tid, || ...)` — `child_tid` is already allocated before this point and, for a real
  process-clone (`fork()`), IS the child's real future `pid` (matches what
  `set_next_spawned_thread_guest_pid` assigns later at spawn time). This makes every claim the
  eager copy registers belong to the CHILD's own future identity instead of the parent's, so two
  concurrently-duplicating siblings are correctly mutually foreign for the whole vulnerable
  window — restoring the exact collision defense that already existed for "two unrelated guest
  processes" to this "two sibling children of the same still-forking parent" case too. Narrowly
  scoped to claim attribution only, per this section's own "narrow it down further" directive —
  does not touch the previously-reverted broader `lock_fork_verify_heal()`-around-the-whole-call
  approach (still correctly reverted, still a worse fix).

  **Verified against the regression oracle**, with real memory headroom explicitly checked before
  trusting the numbers (free physical memory ~1.9–3.7 GiB out of ~16 GiB across the runs below —
  BELOW the ~4 GiB caution threshold this project's own memory already flags as a known confound;
  treat these as probably-real but not iron-clad, and re-verify on a fresh host if revisited):
  FAIL case (30-concurrent-`/bin/true`, baseline 3-6 faults/run): **8/8 consecutive runs at 0
  faults** post-fix. PASS control (was 0/0/0): **3/3 runs still at 0/0/0**, no regression.

  **Independently reverified on a separate host session with healthy memory (~5.7 GiB free,
  above the caution threshold)**: FAIL case **0 faults in 10 CONSECUTIVE runs** (deliberately run
  longer than the original 8 given the earlier memory-pressure caveat), PASS control **0 faults,
  2/2**. Two independent methods (litebox's own VMA-flags table, logged as
  `VM_READ|VM_MAYREAD|VM_MAYWRITE|VM_MAYEXEC` with `VM_EXEC` absent on the crashing range, plus a
  separate count of 108 `PAGE_READONLY` vs. 59 `PAGE_EXECUTE_READ` `VirtualProtect` calls in one
  run; and this session's own `VirtualQuery`-at-fault-time capture,
  `MEM_COMMIT`/`MEM_PRIVATE`/`PAGE_READONLY`) independently agree on the same permissions-mismatch
  symptom this fix addresses. This is a solid, cross-verified fix — confidence is high.

- **NEW: second, distinct crash signature found blocking the standing goal under the real XFCE
  launch load — STILL OPEN, not fixed by the above.** Running the full reproduction command
  (below) against `layer31_direct_fixed.tar` with the fix above in place: weston (`--use-pixman
  --shell=desktop-shell.so`) still renders correctly and `non_black_pixels=2073597` (the full
  1920x1080 desktop signature) is sustained through the FINAL frames of a 100s run — but this is
  STILL the weston-only false-positive this doc's own "standing directives" section warns about:
  XFCE itself never comes up. Direct cause, found by reading the log's own shell `-x` trace
  end-to-end: `xfce_direct.sh` line 11, `dbus-daemon --nofork --nopidfile --nosyslog
  --address="$DBUS_SESSION_BUS_ADDRESS" --session &`, backgrounds a subshell that is killed by a
  fatal signal BEFORE `dbus-daemon` itself ever reaches `execve()` — confirmed by grepping the
  entire run log for any trace of `dbus-daemon` (execve, DIAG_TIMELINE, or otherwise): it never
  appears ANYWHERE. The killed process's own `comm` is still `sh` (the backgrounding subshell),
  `Signal(4)` (SIGILL), `cr2=0x0`/`error_code=0x0` (genuinely-unmapped `#UD`, not a page-fault —
  see this doc's own "Exception(N)/error_code decoding" technique below), and critically **`rip`
  is the SAME fixed value both times this was observed in one run** (`0x7feffff7fb8a`) —
  `pid=7` at 0.68s (the dbus-daemon backgrounding attempt) and again `pid=73` at 35.1s (a later,
  unidentified `sh` subshell during the xfwm4/xfdesktop/xfce4-panel launch sequence). A fixed,
  repeated `rip` across two unrelated fork instances initially looked like a fault-tolerant
  healing helper (`fork_verify`'s own protection widen/restore lines appear right beside both
  crashes in the log) — **checked directly and refuted**: `rip=0x7feffff7fb8a` decimal is
  `140668768353162`, which falls squarely inside the SAME run's own logged `diag-exec-mmap` range
  for `/lib/ld-musl-x86_64.so.1` (`140668768194560`..`140668768559104`, offset `+158602` into the
  mapping) — this is a fault on REAL, in-range musl libc code (very likely `sh`'s own fork/thread
  startup path inside musl, given the two occurrences are both `sh` forking), not a `fork_verify`
  internal at all. The concrete next step is therefore the same class of investigation as the
  now-fixed bug above, but for this different path: capture `LITEBOX_DIAG_FAULT_VQ=1` for THIS
  signature specifically (it won't be caught by the existing `rip==cr2` gate, since here
  `cr2=0x0` while `rip` is the real fault address — the gate condition itself may need widening
  to `rip != 0 && (rip == cr2 || cr2 == 0)` for this class) to see the actual committed/protect
  state of the `ld-musl` page at fault time, then check whether THIS shared library's own
  concurrent-fork placement path (likely still going through `Vmem::duplicate`'s per-region loop,
  same as before, but possibly a DIFFERENT coalescing/grouping edge case than the one just fixed
  — e.g. `ld-musl` may land in its OWN relocation group separate from `/bin/sh`'s main image,
  worth checking `MAX_INTRA_GROUP_GAP`-driven grouping specifically) is similarly vulnerable to a
  same-owner-coalescing collision the just-landed fix does not fully cover. Because `dbus-daemon` never starts, `xfconfd`/`xfwm4`/`xfdesktop`/`xfce4-panel` all still
  launch with no session bus — the log shows zero "Connection refused" lines this run (an
  improvement over the previously-documented full cascade) but also zero evidence any of those
  components did real session-bus-dependent work, since none of their execve/DIAG_TIMELINE lines
  appear either (only weston, weston-desktop-shell, and dbus-uuidgen ever reach execve in this
  run's full log). **The standing goal is NOT met.** This is a DIFFERENT crash signature (fixed,
  non-`cr2` `rip`; `cr2=0` not a real code-page address) from the `rip==cr2`/`PAGE_READONLY` bug
  fixed above — do not assume the same root cause or the same fix applies; investigate
  independently, likely starting inside `fork_verify.rs`'s own fault-tolerant-write healing path
  rather than `Vmem::duplicate`.

  **DETERMINISTIC 30-SECOND REPRO FOUND for this #UD (bisected, do not use the full XFCE stack to
  chase this — use this instead):** the trigger is `set -x` in the launch script, NOT anything
  about dbus-daemon or XFCE specifically. Bisection: starting from a script where `dbus-daemon`
  spawns fine (3/3), adding back only `set -x` (no `LD_LIBRARY_PATH`) reproduces the #UD 2/2;
  adding back only `LD_LIBRARY_PATH` (no `set -x`) stays clean 2/2. `set -x` makes the shell
  write a trace line to stderr before every command, including right around a backgrounded job's
  fork — extra `write()` syscalls interleaved with fork, each running through a patched
  trampoline stub. Working theory: this extra concurrent trampoline traffic during fork is what
  makes a stub fail to decode. Minimal repro going forward: `set -x` + a single backgrounded
  command, no XFCE/weston/display stack needed at all.

  **Methodological warning this bisection surfaces**: `set -x` is present in most of this
  project's own debug/launch scripts written throughout tonight's investigation (including
  `advisor/probes/run_xfce_staged.sh` and probably `xfce_launch.sh` variants). Some portion of
  earlier "XFCE is broken" findings in this session's history may be an observer effect — the
  tracing instrumentation itself crashing the very thing being traced — rather than a genuine
  XFCE/display-path bug. Treat any earlier finding that used a `set -x`-instrumented script with
  appropriate skepticism until reproduced without it. **Action taken**: `set -x` should be
  dropped from launch/debug scripts going forward (replace with explicit `echo` stage markers) —
  but the #UD itself is still a real, worth-fixing litebox bug now that it has a fast deterministic
  repro, not something to just work around by removing tracing.

  **Next measurement, given the fault is now reliably reproducible in ~30s**: dump the trampoline
  stub bytes from BOTH the parent (which survives) and the child (via
  `LITEBOX_DIAG_FAULT_VQ=1`/its rip==cr2-widened variant) and diff them directly. Differing bytes
  = fork is corrupting trampoline stubs during copy. Identical bytes = the child jumped into a
  valid stub at a non-instruction boundary (a different bug class — a jump-target computation
  issue, not memory corruption).

  **Keep this on the list even after it stops blocking the launch chain** (see the major update
  at the top of this section — removing `set -x` unblocked the full XFCE launch without fixing
  this bug). It's a real litebox bug with a fast, deterministic repro, and it silently breaks any
  traced (`set -x`) script — worth fixing properly, not just avoiding.

  **UPDATE (this pass): exact exception decoded, root cause partially found and partially fixed,
  NOT fully closed — the #UD is NOT a plain `#UD` at all.** `LITEBOX_DIAG_FAULT_VQ=1`'s gate
  (`vectored_exception_handler`, `litebox_platform_windows_userland/src/lib.rs`) was `rip == cr2`
  only, which never fires for this signature (`cr2=0` always, for either sub-case below) —
  widened to also catch raw Windows exception code `0xc0000096`
  (`STATUS_PRIVILEGED_INSTRUCTION`), not just `EXCEPTION_ILLEGAL_INSTRUCTION`. Live capture on
  both real crashes this pass: `raw_code=0xc0000096`, `region_base=0x7feffff7f000`,
  `state=MEM_COMMIT`, `protect=PAGE_EXECUTE_READ` — a **real, present, executable page inside the
  trampoline-stub band** (`maybe_patch_exec_segment`'s allocation range, `litebox_shim_linux/src/
  syscalls/mm.rs`), not a plain `#UD`/invalid-byte-decode. `0xc0000096` is Windows' name for an
  unprivileged `hlt` — the EXACT trap musl's mallocng `a_crash()` deliberately executes on a
  detected heap-integrity violation (see this file's own `is_private_data_range` doc comment in
  `litebox/src/mm/linux.rs` for a PRIOR, already-fixed instance of this identical
  `STATUS_PRIVILEGED_INSTRUCTION`/`hlt` signature, caused THAT time by a stale untranslated
  post-fork heap pointer reaching `free()`). This crash is very likely the SAME class of signal
  (mallocng correctly self-detecting real corruption), not litebox generating a bad instruction
  directly — but the corruption source this time is different from that prior fix.

  **A real, confirmed synchronization gap was found and fixed** (uncommitted as of this pass —
  see below): `PageManager::duplicate()`'s eager per-region byte-copy (`litebox/src/mm/linux.rs`)
  reads a forking process's OWN LIVE trampoline-stub memory region with **no lock at all** — not
  even the Windows allocation/protect locks its own writes use. `maybe_patch_exec_segment`
  (`litebox_shim_linux/src/syscalls/mm.rs`) WRITES new stubs into that exact region while holding
  `elf_patch_cache.lock()`, but `do_clone` (`litebox_shim_linux/src/syscalls/process.rs`) never
  took that same lock before calling `duplicate()` — a genuine, unsynchronized data race between
  one thread extending the trampoline (triggered by `set -x`'s extra `write()` syscalls, each one
  executing through a trampoline stub) and a different thread's concurrent `fork()` reading that
  same memory. **Fix applied**: `do_clone` now holds `self.global.elf_patch_cache.lock()` for the
  duration of the `duplicate()` call (dropped immediately after, before relocation-map
  merging/fd-table duplication, which don't need it).

  **Verification result — fix is real but INCOMPLETE, do not claim this bug is closed:**
  - The isolated fast repro (advisor's bisected `set -x` + single backgrounded `dbus-daemon`,
    `alpine-pinned2.tar`): **6/6 clean runs post-fix** (was reliably 2/2 FAIL pre-fix).
  - The 30-concurrent-fork regression oracle (the OTHER, earlier-fixed bug's own oracle): **3/3
    clean**, no regression.
  - The FULL `xfce_direct.sh` launch (still has `set -x`, `layer31_direct_fixed.tar`): **still
    crashes, bit-for-bit identical signature** (`rip=0x7feffff7fb8a`, `pid=7`/`73`, same two
    timestamps ~0.5s/~36s) even with this fix applied. An isolated repro built from the SAME tar
    (`layer31_direct_fixed.tar` instead of `alpine-pinned2.tar`) but only running the dbus-daemon
    lines from `xfce_direct.sh` (not the full script) stayed clean, meaning the full script's
    heavier concurrency (more background services, more forked children, more trampoline
    extension traffic) still finds a window this specific lock does not close — likely a second
    reader of the trampoline region that also bypasses `elf_patch_cache` (candidate: `fork_verify`'s
    own single-step/AV-path healing reads code bytes via `read_code_bytes`,
    `litebox_platform_windows_userland/src/fork_verify.rs`, also with no `elf_patch_cache`
    coordination) or a race window inside `duplicate()`'s multi-step
    allocate-then-copy-then-protect sequence that a single outer lock around the whole call does
    not fully serialize against a writer that also needs to allocate more trampoline pages
    mid-race (`maybe_patch_exec_segment`'s `do_mmap_anonymous` growth path, `litebox_shim_linux/
    src/syscalls/mm.rs` ~line 1530).
  - **Next step if resumed**: per the diff-the-stub-bytes suggestion already in this section,
    capture the PARENT's copy of the same trampoline region (via `LITEBOX_DIAG_FAULT_VQ`'s
    already-added, now `0xc0000096`-aware capture, extended to dump N bytes at `rip` not just
    `VirtualQuery` metadata) and diff against the CHILD's corrupted copy on a repro that still
    fails post-fix, to confirm whether the corruption is still a torn trampoline write (this fix's
    own hypothesis, apparently still not fully closed) or something else the evidence has not yet
    distinguished.

  **Priority note**: superseded as the launch-blocking issue by "Rendering/scanout blocker" below
  (removing `set -x` from the launch script sidesteps this bug entirely and the desktop now comes
  up) — this bug is no longer standing between the session and the standing goal, but is still a
  real, reproducible litebox bug silently breaking any `set -x`-traced script, worth closing
  properly if picked up again.

## Rendering/scanout blocker (the current single remaining gap)

With both concurrent-fork process bugs fixed and `set -x` removed from the launch script, a full
XFCE desktop now starts and stays alive (weston, Xwayland, xfconfd, xfwm4, xfsettingsd,
xfdesktop, xfce4-panel — see the major update at the top of this section for exact confirmation).
**The only remaining problem is that the framebuffer goes black and never recovers, independent
of XFCE or `xfwm4` entirely:**

- t=7.4s: `non_black_pixels=2073597`, `colors=64` — weston renders a full desktop correctly.
- t=20.3s: `non_black_pixels=0`, `colors=1` — goes black, never recovers for the rest of the run.
- The blackout coincides with Xwayland (not yet running any XFCE component) forking `xkbcomp`
  after a large (46,327-pointer) fork_verify healing pass.
- Every XFCE component starts from t=29.9s onward — well AFTER the blackout. Neither XFCE nor
  `xfwm4` causes this; it's already black before any of them exist.

This is now a compositing/scanout question, not a process-launch question: does Xwayland's output
ever reach weston's scanout buffer, or does weston stop flipping once Xwayland becomes the top
surface?

**MEASUREMENT DONE: `LITEBOX_DRM_TRACE=1` (commit `a6d6ba55`, corrected call path in `37cf16fb`)
answers the "did weston stop flipping" question decisively — it did NOT.** 67 DRM ioctls
captured on a full XFCE run, 27 `DrmModePageFlip`. Critical correlation:
- Frames go BLACK at t=19.63.
- Page flips CONTINUE at t=19.74, 19.99, 20.10 — AFTER the blackout.
- 27 page flips == 27 captured `LITEBOX_DUMP_FRAMES` frames exactly — no missed-frame/capture
  artifact; every flip is faithfully observed.

**The guest IS flipping buffers — the buffer CONTENTS are empty.** This rules out "weston
stopped presenting" entirely; the mechanism is a buffer-content problem, not a flip-scheduling
problem.

**Sharper timing pattern**: flips are not evenly spaced.
- t=7.78–8.15: nine flips in ~0.4s (weston-desktop-shell's own render, `2,073,597` px each — the
  already-confirmed-working weston path).
- t=8.15 → t=19.10: **an 11-SECOND GAP with ZERO flips at all.**
- t=19.10, 19.63, 19.74, 19.99, 20.10: flips RESUME, now BLACK, never recovers.

The gap begins the moment Xwayland `exec`s (pid 17 at t=9.78) and ends around when Xwayland forks
its `xkbcomp` helper (pid 21 at t=18.80). Sequence: weston renders its own shell fine → Xwayland
starts and weston stops flipping ENTIRELY for 11s → flipping resumes with an EMPTY buffer and
never recovers. Reads as Xwayland taking over the output and never producing real content — not
anything XFCE does (every XFCE component starts after t=29, well past this whole sequence).

**MEASUREMENT DONE (commit `9c4dd998`): fb-id logging gives a definitive answer — content loss
on EXISTING buffers, not a surface-ownership swap.** weston double-buffers between `fb_id=1` and
`fb_id=2` for the whole run. Correlating each flip's fb id against that frame's pixel count:
```
t=7.07 - 7.81   fb 1,2,1,2,...   px=2,073,597   good
t=18.34         fb_id=1          px=2,073,597   still good
t=18.89         fb_id=2          px=0           BLACK
t=19.28         fb_id=2          px=0           BLACK
t=19.42         fb_id=1          px=0           BLACK
```
`fb_id=1` renders `2,073,597` pixels at t=18.34 and `0` pixels at t=19.42 — the SAME framebuffer,
contents gone 1.1 seconds later. **No third framebuffer ever appears — rules out
surface-ownership handoff entirely.** Combined with flips continuing throughout (established
above), the mechanism is: **the existing dumb buffers' contents are being zeroed or their mapping
lost, while the DRM bookkeeping stays perfectly valid.** weston keeps flipping the same two fbs;
they simply no longer contain what weston drew.

**Matches an already-fixed bug class in this project**: same shape as the `memfd` mmap-time wipe
bug fixed earlier this session (commit `3a0755e4`) — a shared object's contents overwritten with
zeros behind a live mapping — just on the DRM dumb-buffer path instead of `wl_shm`. Timing fits:
the zeroing happens right as Xwayland starts up and forks, exactly when the memory subsystem is
busiest.

**Concrete suspects, in priority order**:
1. Anything that re-creates/re-commits the dumb buffer's backing memory while a mapping is still
   live — check for a DRM-dumb-buffer equivalent of the memfd "copy the Vec over the shared
   object" bug (see the fixed memfd bug for the exact pattern to look for).
2. CoW/fork interaction with `MAP_SHARED` dumb buffers: weston maps the scanout buffer, a fork
   happens nearby (Xwayland/xkbcomp). If a shared scanout mapping gets treated as private and
   copied during fork, the compositor keeps writing into a copy while scanout reads the
   original — exactly matches "flips continue, content frozen then lost." Check whether
   `PageManager::duplicate()` (already touched twice tonight for unrelated fork races) handles a
   `MAP_SHARED` dumb-buffer mapping correctly during fork.
3. Decommit-then-recommit on the buffer range — a recommitted page comes back zeroed, matching
   `px=0` exactly (not garbage) far better than a corruption theory would.

**MEASUREMENT DONE (commit `80590825`): the buffer is genuinely wiped at the SOURCE — airtight,
eliminates every alternative.** Sampled the scanout buffer's real shared backing store directly
at each flip (fresh `map_shared_memory(handle)` every time, never a stale view):
```
t=7.42-8.16  fb 1,2,1,2...  nonzero=6,221,884  first8=[23,11,0,255,...]  real content
t=19.68      fb=1           nonzero=6,221,884  first8=[23,11,0,255,...]  still good
t=20.27      fb=2           nonzero=0          first8=[0,0,0,0,0,0,0,0]  WIPED
t=21.07      fb=1           nonzero=0          first8=[0,0,0,0,0,0,0,0]  WIPED
```
`fb=1` holds `6,221,884` non-zero bytes at t=19.68, exactly ZERO at t=21.07. **This rules out**:
capture-path bug (fresh mapping each read), CoW/private-mapping divergence (reading the shared
object itself), surface ownership (no third fb ever appears), compositor-stopped-presenting
(flips continue throughout).

**The signature is decisive**: buffers read EXACTLY zero, not garbage. Freshly-committed pages
read as zero; corrupted/reused memory reads as garbage. This means the buffer's pages are being
DECOMMITTED AND RECOMMITTED, or the shared section is being replaced/recreated, while DRM
bookkeeping (fb ids, handles, flip path) stays perfectly valid — exactly why everything
downstream still looks healthy. Same class as the already-fixed `memfd` mmap-time wipe bug, now
on the DRM dumb-buffer path.

**Critical narrowing**: the wipe window (t=19.68 to t=20.27) contains NO DRM ioctl at all — only
205 `diag-vprotect` entries. Nothing in the DRM path itself wipes it; the memory subsystem does,
during heavy protection churn while Xwayland is starting/forking.

**Where to look, in priority order (not yet instrumented)**:
1. Any path that recreates or resizes a shared-memory object for an EXISTING handle — the memfd
   fix's analogue (whatever `resize_memfd_shared_backing`-equivalent may exist for DRM dumb
   buffers, for the exact same shape as the already-fixed bug).
2. Whether a decommit/recommit ever touches the dumb buffer's address range — a recommit produces
   exactly-zero content, matching the signature precisely.
3. `Vmem::duplicate`'s shared-mapping branch — read directly and appears correct (re-maps the
   same handle rather than copying), but not yet instrumented/proven; verify rather than trust.

**Next measurement, not yet done**: log create/resize/destroy of shared-memory objects with their
handle, then grep for the dumb buffer's specific handle in the t=19.7-20.3 window. If that
handle's underlying object gets recreated there, that's the bug, found directly.

**FURTHER NARROWED (not yet confirmed — needs instrumentation, do not trust a code read here)**:
measured facts: (1) the scanout buffers are created ONCE at t=5.98 (2×`DrmModeCreateDumb` +
2×`DrmModeMapDumb`), no `DestroyDumb`/`RmFB`/re-create for the whole run — kills the
"object recreated" theory outright. (2) The wipe window (t=19.68→20.27) contains ZERO DRM
ioctls, only 205 memory ops, 197 of them `caller=fork_duplicate` — the fork is Xwayland forking
`xkbcomp` at t=19.37. (3) Some `fork_duplicate` ranges are framebuffer-shaped (82,944,000 bytes =
exactly 10×1920×1080×4). (4) **Across the ENTIRE run, 9,304 `diag-protect-mapping` entries report
`vma_shared=false` — not one `vma_shared=true` anywhere** (`vma_shared` =
`vma.shared_handle.is_some()`).

**Lead**: if weston's DRM-dumb-buffer VMA has no `shared_handle` attached, `Vmem::duplicate` at
fork takes the non-shared branch and EAGERLY COPIES the region into fresh pages instead of
re-mapping the same handle — fresh pages read as exactly zero, matching the measured signature
precisely, and explains the timing (wipe only ever happens at a fork).

**Caveat, stated honestly, do not skip verifying this**: the forking thread at the wipe moment is
Xwayland's (weston is pid 13, Xwayland's fork is a different thread), so the absent
`vma_shared=true` might just mean the diagnostic never fires on WESTON's own mapping at all — not
that the mapping genuinely lacks a `shared_handle`. Zero logging currently exists on
`map_shared_memory`/shared-handle attachment to settle this either way.

**CHECKED AND REFUTED**: logged the VMA at creation —
`diag-shared-vma-created flags=123 has_handle=true is_shared=true`,
`diag-drm-dumb-mmap len=8294400 offset=4096`. `flags=123` =
`VM_READ|VM_WRITE|VM_SHARED|VM_MAYREAD|VM_MAYWRITE|VM_MAYEXEC`, handle present, size exactly
`1920*1080*4`. **The mapping IS correctly shared.** Fork takes the shared branch as expected — the
"eager copy into fresh zeroed pages" theory does NOT apply. Do not re-investigate this. (The
`vma_shared=false`-everywhere observation was a red herring: that diagnostic simply never fires
on this VMA at all — an instrumentation gap, not evidence of a missing handle.)

Also checked and refuted: two new shared objects DO get created right at the blackout (handles
576/580 at t=20.62/20.77), but they are 82,944,000 and 36,864 bytes — NOT framebuffers. The
capture reads `bytes_len=8,294,400` on every single flip (always the original 8.29MB objects), so
no buffer-swap is happening either.

**What's now solid, reproduced across three runs**: the SAME 8.29MB buffer objects are read
throughout the whole run (`bytes_len` constant, never changes); buffers created once at t=5.94,
never destroyed, never re-created; no DRM ioctl in the wipe window (197 of 205 ops there are
`caller=fork_duplicate`); **weston forks exactly once, at t=6.52, LONG before the wipe — weston's
own fork is innocent** (the wipe happens during Xwayland forking `xkbcomp`, a COMPLETELY
DIFFERENT process from the one owning the mapping).

**MEASUREMENT RETRACTED then CORRECTED AND RE-CONFIRMED (see below) — the reclaim/decommit path
IS excluded, this time on solid footing.** The original overlap test
(`advisor/probes/correlate_scanout_wipe.py`, commit `dcae67c8`) correlated destroy ranges against
the address where the CAPTURE mapped the buffer — but `notify_flip_callback` calls
`map_shared_memory` FRESH on every flip, reads, and unmaps immediately, so that mapping exists
for microseconds. Finding nothing destroys THAT is nearly tautological; it never tested weston's
own actual, long-lived mapping. Tell in the data: `fb_id=1` was logged at four DIFFERENT
addresses across different flips (`1620770816`, `2126577664`, `4470276096`, `743571456`) — a
stable framebuffer does not move; those were all transient capture mappings, not weston's real
one. **weston's actual persistent mapping address was never logged and the correct overlap test
has not been run.**

**FIXED AND RE-RUN — this time a real, valid result: the reclaim/decommit path is genuinely
excluded.** Logged the GUEST's real persistent scanout mapping addresses and re-ran the overlap
test against them (not the transient capture mapping):
```
GUEST persistent scanout mappings:
  0x1ef70000-0x1f759000 (8294400 bytes)
  0x1fad0000-0x202b9000 (8294400 bytes)
19,063 reclaim/decommit events
-> no destroy event overlaps EITHER guest scanout mapping
```
This is a valid negative this time, not the near-tautology from before. **The reclaim and
decommit paths genuinely do NOT touch the scanout buffers.**

**What still stands, unaffected by the retraction above**: same two shared objects (508, 512) for
the whole run, created once, never destroyed (no `DestroyDumb`/`RmFB`); VMA correctly shared
(`flags=123`, `has_handle=true`); fb→handle mapping stable (`fb_id=1`→handle 508 set once at
t=7.55, `fb_id=2`→handle 512 set once at t=6.74, neither ever changes, verified by address
matching); weston (pid 13) and weston-desktop-shell (pid 15) both alive to the end, zero exits;
system stays busy afterward (7,297 socket ops after t=25). **And yet contents still go from
6,221,880 non-zero bytes to EXACTLY zero.** Whether an explicit destroy/decommit call is
responsible is UNRESOLVED (see retraction above) — not excluded, not confirmed.

**"Not a memory bug" REFRAMING ABOVE IS ITSELF REFUTED — confirmed genuine memory bug, decisive
detail found.** Per-flip content digest + non-zero byte count (commit `71cd2ee8`):
```
t=6.67-7.50  nonzero=2,073,600   buffer at creation
t=7.63-8.36  nonzero=6,221,895   weston actively drawing (digest changes every flip)
t=18.93      nonzero=6,221,895   last real content
t=19.53+     nonzero=0           BLACK
```
**Decisive detail**: `2,073,600` = exactly `1920*1080` = one non-zero byte per pixel — an OPAQUE
CLEARED framebuffer (`alpha=255`, RGB zero), the buffer's state at creation. `6,221,895` ≈ 3
bytes/pixel = real color content. `0` = not even the alpha channel. **The final state is
STRICTLY EMPTIER than the buffer's own initial state.** weston never writes an all-zero buffer —
even a fully black desktop keeps alpha set (t=6.67's state). A compositor that merely stopped
drawing would leave the CLEARED state behind (`nonzero=2,073,600`), not reach `nonzero=0`. **The
memory really is being zeroed.** (Caveat: the digest samples every 4096th byte, so an identical
digest between states is a hash collision, not proof of identical state — the non-zero BYTE COUNT
is the real distinguishing evidence, not the digest.)

**Where this leaves us, all explicit destroy paths now excluded**: something zeroes the 8.29MB
shared section — never destroyed, correctly shared (`flags=123`, `has_handle=true`, stable
fb→handle mapping), not touched by any decommit/unmap (0 overlaps in 19,133 destroy events) —
while another process forks. The remaining candidates are IMPLICIT (don't go through an explicit
destroy call at all): a fresh `VirtualAlloc2` with `MEM_COMMIT` over the SAME address, a
`MEM_RESET`, or a section view being re-established/re-mapped.

**MEASUREMENT DONE (this session's own instrumentation, independently converged with advisor's
identical fix): `diag-commit` added at all `VirtualAlloc2(MEM_COMMIT)` call sites
(`litebox_platform_windows_userland/src/lib.rs`, both inside the `reserve_and_commit` closure and
the fixed-address-loop's direct commit-over-`MEM_RESERVE` branch), same field format as
`diag-reclaim`/`diag-decommit` (`start=`/`end=`/`len=`). Also independently added
`diag-drm-fb-addr` logging at the GUEST's own `try_dri_dumb_buffer_mmap` success point
(`litebox_shim_linux/src/syscalls/mm.rs`, ~line 646) — confirmed the same persistent guest
addresses advisor found (`519503872`/`0x1EF00000`..`527798272` and
`531431424`/`0x1FA00000`..`539725824`), logged exactly once per run at ~t=6s and never again for
the buffer's whole lifetime.**

**Result, re-verified across 2 complete runs that both instrumented ALL THREE paths (`diag-commit`,
`diag-decommit`, `diag-reclaim`) AND reached the wipe window (one wipe at t=19.30→19.82, another at
t=18.34→18.84): ZERO overlaps with either guest scanout address range, for ANY of the three memory
APIs, across the ENTIRE run** (not just the wipe window — checked in full, ~2,000-7,700 events per
path per run). This independently confirms and extends advisor's `correlate_scanout_wipe.py`
result: it is not merely that decommit/reclaim don't touch the buffer, `VirtualAlloc2(MEM_COMMIT)`
doesn't either. **Every Windows-level memory-management API this project can instrument
(decommit, unmap, reclaim, and now commit) is now excluded as the wipe mechanism.**

**Also checked and dead-ended**: (1) `fork_verify`'s `write_usize_fault_tolerant` (the single-step
stale-pointer healing write path) only ever writes one `usize` (8 bytes) per fault — structurally
cannot explain a ~6MB content loss even under a hypothesized systematic mistranslation, and its
target addresses are individually fault-driven, not a bulk range write; not instrumented further
because the mechanism itself is the wrong shape for this signature. (2) `close_shared_memory` is
never called on the buffer's handle in any run (zero `diag-shm: close_shared_memory` log lines) —
rules out handle-value reuse/collision via an errant close. (3) Confirmed via a dedicated subagent
that ALL guest "processes" (weston, Xwayland, xkbcomp, etc.) run as `std::thread`s inside ONE real
Windows host process — `fork()`/`clone()` goes through `Vmem::duplicate` (`litebox/src/mm/linux.rs`),
never real `CreateProcessW` (that path, `litebox_platform_windows_userland/src/process_fork.rs`, is
explicitly diagnostic-only, gated behind `LITEBOX_DIAG_PROCESS_FORK_SPAWN=1`, and its spawned child
is `TerminateProcess`'d immediately without running real guest code). This rules out a
cross-real-process HANDLE-value collision (no second real handle table exists), but leaves open
whether two DIFFERENT guest processes' own *logical* `Vmem`s (weston's and Xwayland's, each
believing it owns its own guest-virtual address space) could pick the SAME real underlying Windows
host address for unrelated allocations without either one's bookkeeping ever knowing — this was
dispatched to a subagent for investigation (see `CLAIMED_RANGES`,
`litebox_platform_windows_userland/src/lib.rs` ~line 4018-4370) but not yet resolved as of this
writing; the next session should read that subagent's report (or re-run the investigation if it
did not complete) before opening new avenues.

**Operational note for future sessions**: host memory pressure is a real, live confound.
`Get-CimInstance Win32_OperatingSystem` showed ~4.9GB free out of 16GB total during this session
with two parallel investigating sessions plus their subagents/background runs all launching XFCE
concurrently; at least one run in this session and one in advisor's died early
(`memory allocation of ... bytes failed`, or simply stalled with no further output) well before
ever reaching the t≈19s wipe window. A run that dies/stalls early looks identical to "no wipe
occurred" to a naive oracle — always check that a "clean" run's own log actually reaches
`diag-drm-flip-source-bytes` entries past t≈20s before trusting a negative/absent-overlap result
from it. Check free memory before launching another full XFCE run if multiple sessions are active.

**FAST REPRO FOUND (~16 seconds, not the full ~500s XFCE launch) — use this for iteration going
forward**: three-way isolation, each run under 40 seconds:
```
weston alone + 20 backgrounded forks     -> NO wipe. 25 frames, ends at 2,073,597 px.
weston + Xwayland, nothing connects to X -> NO wipe. 24 frames, ends at 2,073,597 px.
weston + Xwayland + ONE X client         -> WIPE at t=16.4, nonzero 6,221,890 -> 0
```
**Forks alone do NOT trigger it. Xwayland merely running does NOT trigger it. It needs an X
CLIENT to actually connect.** That connection is what makes Xwayland fork `xkbcomp` (2 execs in
the failing repro, 0 in the no-client passing run), and `xkbcomp`'s fork carries a ~46,000-pointer
heal versus a few hundred for an ordinary shell fork. Repro recipe: start seatd, start weston
(drm-backend, pixman, desktop-shell), start Xwayland `:1` fullscreen, wait for
`/tmp/.X11-unix/X1`, then run any X client (`DISPLAY=:1 xfce4-about --version` works) — no dbus,
no xfconfd, no XFCE session, no window manager, no panel needed at all.

**Every explicit memory operation now instrumented, NONE of them touch the scanout buffers**:
scanned every logged operation carrying a start/end range against the two guest scanout mappings
in the fast repro — `diag-commit` (`VirtualAlloc2`/`MEM_COMMIT`): 0 overlaps; `diag-reclaim`/
`diag-decommit`: 0 overlaps; `diag-vprotect`: 2 overlaps, both at t=0.97 (creation only);
`DrmModeMapDumb`: 2 total, both at startup; guest re-mmap of the buffer: none after t=0.97.
**Nothing we currently log touches the scanout buffers between creation and the wipe, yet their
contents go to exactly zero.** This is itself a strong clue: whatever zeroes it is not going
through any currently-instrumented mapping-level path — it must be a WRITE, not a
map/commit/decommit/protect operation.

**Candidates, in priority order, none yet confirmed**:
1. The fork/duplicate path writing INTO the child at addresses that alias the parent's shared
   section. All guest processes share one real Windows address space, so a relocation computing
   a wrong destination could land on the framebuffer without any decommit/commit ever being
   logged — it would just look like an ordinary memcpy. Ties directly into the still-open
   cross-`Vmem` real-address-collision question two paragraphs above.
2. `fixup_stale_elf_data_pointers`/fork_verify's own healing writing through a stale pointer. The
   ~46,000-pointer heal during `xkbcomp`'s fork is the largest such operation in the whole run,
   and it's exactly what distinguishes the failing case from the two passing ones.
3. Anything that zero-fills a BSS or new mapping using a length or base computed from the wrong
   VMA.

**`LITEBOX_FORKVERIFY_OFF=1` bisection TRIED — INCONCLUSIVE, do not repeat.** Disabling reactive
healing breaks the guest well before the point of interest: run dies at t=3.2 (`exit=11`) having
only reached seatd startup — weston never starts, Xwayland never starts, no X client, no frames
captured at all. Two guest faults on the way down, the second (`rip=0x7feffff80233`) in the
trampoline band again. This cannot distinguish the heal-path candidate from the other two; the
test itself is broken, not the hypothesis.

**Decisive next measurement, not yet done — use one of these, not the FORKVERIFY_OFF bisection**:
1. **`GetWriteWatch`** (Windows API, exactly for this): allocate the scanout buffers with
   `MEM_WRITE_WATCH`, poll `GetWriteWatch` at each page flip, log which pages were dirtied since
   the previous flip. During normal rendering this should show weston's own drawing; at the wipe
   it will show whoever actually wrote there — the dirtied page addresses alone will usually
   identify the writer directly.
2. **Cheaper, deliberately destructive trap**: after the last known-good flip, `VirtualProtect`
   the scanout range to `PAGE_READONLY` and let the offending write raise an access violation —
   the existing VEH captures the faulting `rip` directly, naming the writer in one run. Not a
   fix, a probe, but names the exact call site fast.

**Caution for whoever runs this**: correlate against the GUEST's own PERSISTENT mapping, not any
per-flip capture mapping — the capture maps the section fresh and drops it within microseconds,
and the same fb has been observed at four different addresses across different flips. Correlating
against the capture mapping gives a false negative that looks convincing (this exact mistake was
made and retracted earlier in this investigation).

Standing facts for whoever picks this up: repro is seatd, weston (drm/pixman/desktop-shell),
Xwayland `:1` fullscreen, then ONE X client (`advisor/probes/scanout_wipe_repro.sh`); wipe at
t~16, `nonzero 6,221,890 -> exactly 0`; zero overlaps against the scanout range across
`diag-commit`/`diag-reclaim`/`diag-decommit`; only two `diag-vprotect` hits on that range in the
whole run, both at creation (t=0.97) — nothing instrumented touches the buffer between creation
and the wipe.

**Independent second-session confirmation (this session), fully converged with everything above,
plus new findings, dead ends, and a corrected fast-repro fact**:

- Re-ran the full three-metric overlap check (`diag-commit`, `diag-decommit`, `diag-reclaim`)
  against the GUEST's own persistent scanout addresses (independently re-derived and logged at
  `litebox_shim_linux/src/syscalls/mm.rs`'s `try_dri_dumb_buffer_mmap` success point — confirmed
  identical addresses to advisor's own: `0x1EF00000`/`519503872` and `0x1FA00000`/`531431424`,
  logged exactly once per run at ~t=6s and stable for the buffer's whole lifetime) across two full
  XFCE runs that both reached the wipe (t=19.30→19.82 and t=18.34→18.84). **Zero overlaps for all
  three APIs, for the entire run, not just the wipe window** — same result as advisor's, reached
  independently. This is now confirmed by two separate sessions using two separately-added,
  independently-verified logging sites.
- **Corrected fast-repro fact**: built a minimal fast-repro script (seatd → weston → Xwayland `:1`,
  no X client at all) and reproduced the wipe in ~13.6s, WITHOUT ever running an X client —
  contradicting advisor's "needs an X client to connect" finding above. Xwayland merely running
  long enough (past `XWAYLAND_READY`) is sufficient to trigger it in this session's runs; whether
  advisor's original 3-way isolation result was itself timing-sensitive (i.e. the "no wipe" cases
  simply hadn't run long enough yet) is unresolved — the isolation experiment should be re-run with
  a longer timeout before trusting "needs a client" as a real precondition.
- **`LITEBOX_FORKVERIFY_OFF=1` bisection independently re-attempted and independently reached the
  same inconclusive result** as advisor's own attempt above (both sessions tried this without
  seeing each other's result first): with fork_verify's reactive healing disabled, the run dies
  with a genuine host-side `#PF` (`Exception(14)`, `cr2` genuinely unmapped) partway through
  startup — this run never even got weston running, let alone Xwayland or a client. **Two
  independent attempts, two independent confirmations that this bisection is unusable** — do not
  attempt it a third time; fork_verify's healing is load-bearing for basic process-launch
  stability, and disabling it does not isolate the wipe question, it just substitutes a different,
  earlier, already-documented crash.
- **Traced but did NOT find evidence of exploitation**: `CLAIMED_RANGES`
  (`litebox_platform_windows_userland/src/lib.rs` ~line 4018-4370), the registry that prevents two
  different guest processes' `Replace`-mode (fixed-address) allocations from colliding on the same
  real Windows address, is populated ONLY from `allocate_pages` calls (`claim_range`, called at
  lines ~4332/5729/5797) — it is NEVER populated by `map_shared_memory` (confirmed via `grep`: zero
  `claim_range` call sites in `map_shared_memory`/`create_shared_memory`). This means weston's DRM
  scanout buffer's real address is structurally INVISIBLE to `CLAIMED_RANGES` — a genuinely
  existing gap, not a hypothetical one. However: `Replace`-mode's OTHER collision guard
  (`has_committed_page`, a direct `VirtualQuery` against Windows' own real VAD state, checked
  before `find_foreign_claim` even runs) WOULD still see the buffer's real `MEM_COMMIT`/`MEM_MAPPED`
  state correctly regardless of `CLAIMED_RANGES` — so this gap is not immediately exploitable by
  itself. Whether some code path could still race past `has_committed_page`'s check (a TOCTOU
  window, or a `Hint`-mode allocation that never queries commit state for a NULL-hint request) was
  not fully resolved; this is real remaining uncertainty, not a dead end, but no live evidence of
  it firing was found in any instrumented run (would show up as a `diag-reclaim`/`diag-commit`
  overlap, and none were found).
- **Two weston-upstream-source hypotheses investigated via subagents against real weston source
  (gitlab.freedesktop.org/wayland/weston), both DEAD-ENDED**:
  1. `drm_rb_discarded_cb()`/`pixman_renderer_resize_output()` creating a fresh (genuinely
     all-zero-including-alpha) dumb buffer on an output resize/mode-change (`backend-drm/drm.c`,
     `pixman-renderer.c`) — directly refuted against this session's own logs: the wipe window in a
     confirmed-wiped run contains **exactly one `DrmModeSetCrtc` ioctl in the ENTIRE run, at t=6.09s
     (initial setup)**, none anywhere near the wipe (t=18.3-18.8s), and the SAME `fb_id`/buffer
     `handle` values (4556/4560) are used both immediately before and immediately after the wipe —
     no new buffer was ever created, ruling out this mechanism for the observed data.
  2. weston's pixman renderer or damage-tracking legitimately/buggily zero-filling the WHOLE output
     (as opposed to real content or a solid background color) via some client-buffer-attach-failure
     or damage-computation edge case — refuted by direct source reading:
     `pixman_renderer_repaint_output()` scopes both `repaint_surfaces()` and `copy_to_hw_buffer()`
     strictly to `output_damage`, never the whole buffer; `draw_view()` SKIPS compositing entirely
     (a no-op, leaving existing content untouched) when a view has no buffer attached, rather than
     clearing that region to zero. No `PIXMAN_OP_CLEAR`/memset-to-zero path exists in the renderer
     for a stalled/hung client. **weston's own real compositing code structurally cannot produce a
     whole-output, all-channel-zero frame while continuing to flip real fb ids** — this is a strong
     negative result, not merely an unconfirmed one.
- **Bottom line after two independent full sessions' worth of instrumentation**: every mechanism
  either session could name AND instrument has been checked and excluded (Windows memory
  management in full; weston's own real compositing/resize logic in full; fork_verify's own write
  path, wrong shape for the data volume; handle-value reuse, never closed; cross-real-process
  handle collision, structurally impossible in this architecture). The `CLAIMED_RANGES` gap above
  is the one item that is genuinely still open rather than excluded, but has zero supporting
  evidence from any run. **The honest state is: the wipe is real, reproducible in ~14-20s via the
  fast repro, and its mechanism is not visible to any currently-instrumented logging path** — it is
  a WRITE (not a map/commit/decommit/protect operation), it originates during Xwayland's presence
  (not necessarily its fork specifically — see the corrected fast-repro fact above), and finding it
  now requires either `GetWriteWatch`-style live write observation (does not work here — `MEM_WRITE_
  WATCH` is incompatible with `MapViewOfFile3`-backed section views, only works on private
  `VirtualAlloc`-committed memory, so this specific tool is NOT usable for a shared-section-backed
  buffer like this one, a correction to option 1 below) or the `VirtualProtect(PAGE_READONLY)` +
  existing-VEH write-trap approach (option 2 below), which was not attempted this session due to
  the risk of destabilizing the existing, delicate VEH/fork_verify interaction without enough
  remaining session budget to verify it doesn't regress anything — this is the precise, concrete
  next step for whoever picks this up next, not a vague "needs more investigation."

**PROXIMITY check tightens this further, AND opens a new possibility this whole section had not
seriously considered — read before committing to GetWriteWatch/trap work.** Checked every logged
range operation during the tight 450ms wipe window for PROXIMITY, not just overlap: not one range
comes within 16MB of either buffer (436 commits, 404 reclaims, 280 protect-mappings, 197
`fork_duplicate` ops in that window, none anywhere near the framebuffer). The window itself:
```
t=15.48  Xwayland forks, pid 16 execs xkbcomp
t=15.78  xkbcomp exits status=0
t=15.97  LAST GOOD flip, nonzero=6,221,890
t=16.42  FIRST BLACK flip, nonzero=0
t=16.60  Xwayland forks AGAIN, pid 18 execs xkbcomp
t=16.67  second xkbcomp exits status=0
```
Confirms the write does not come from address-space bookkeeping — it comes through a mapping,
which range logs structurally cannot see.

**Possibility this may not be a memory bug at all, reconsidering the earlier "exactly zero"
argument**: two `xkbcomp` forks within 1.2s means Xwayland set up its keymap twice — in a Wayland
compositor, a client appearing and resulting surface/output changes routinely cause a repaint,
and a repaint of a scene with no visible content legitimately clears the framebuffer to zero. The
earlier argument ("weston would leave alpha set on a real clear, so exact-zero proves corruption")
is weaker than it looked: pixman clearing to TRANSPARENT BLACK writes all-zero INCLUDING alpha —
only the DRM dumb-buffer ALLOCATION path produces the opaque-black initial state seen at t=6.67.
So the differing states (opaque-black at creation vs. fully-zero at wipe) do NOT actually rule out
weston legitimately clearing the buffer during a normal repaint.

**MAJOR REFRAME: the flip cadence itself was misread, and this changes the whole shape of the
investigation.** Full flip timeline (not just the few flips immediately after the wipe) across
three independent runs shows weston is NOT continuously rendering — it flips in short bursts when
something changes, then goes completely idle for 10+ seconds, and eventually stops flipping
altogether entirely. That is CORRECT compositor behavior (no damage, no repaint), not evidence of
anything broken:
```
xt1: 29 flips, first t=1.51, LAST t=25.45 -- run continues to t=85 with NO further flips
     gaps: t=2.8->15.3 (12.5s), t=16.8->25.4 (8.5s)
xc1: 30 flips, last t=28.1, gaps of 13.0s and 10.6s
xfM: 25 flips, last t=48.8, gaps of 10.6s and 28.6s
```
The real sequence: t=1.5-2.8 weston paints its shell, flips actively (6,221,880 non-zero bytes).
t=2.8-15.3: IDLE, no flips at all — nothing happening to the framebuffer during this whole
window. t=15.3: Xwayland/xkbcomp activity causes a repaint. t=15.97: last flip, STILL showing old
content. t=16.42: next flip, ZERO. **This is consistent with "weston repaints a scene that now has
nothing visible in it, and paints it to zero" (candidate (c), legitimate clearing) rather than
"content is drawn, then something corrupts it mid-life."** The earlier "weston would leave alpha
set on a real clear" argument against (c) does not hold (see the prior reconsideration above) —
this reframe makes (c) the LEADING hypothesis, not an outsider.

**A more concrete, likely more directly user-facing bug found in the same investigation**: a
trivial X client (`xfce4-about --version`, which should print a version string and exit in
milliseconds) instead runs for 60+ SECONDS without exiting — still alive at the end of a
repeated-client test, actively allocating shared memory and exchanging Wayland protocol at t=25.
**This would fully explain the user's ORIGINAL reported symptom** ("we saw a blue bar, clock and
icon display... then went black" after "a pretty long wait") — the X clients are alive but
pathologically slow, so almost nothing gets drawn in reasonable time, independent of any
memory/scanout question at all.

**Revised priority order, redirect here first**:
1. **Why does a trivial X client take 60+ seconds of wall time instead of milliseconds?** Profile
   where it spends its time. This is likely the actual "XFCE is slow and mostly blank" cause and
   is more directly tied to the original user-reported symptom than the scanout-zero question.
2. Only after (1) is answered: whether the zero-buffer is weston correctly painting an empty
   scene. Cheap test: run an X client that actually MAPS A WINDOW (not just connects and exits)
   and see whether content appears. If a real window renders, there is likely no memory bug here
   at all.

**Hold `GetWriteWatch`/trap plumbing until (1) and (2) are answered** — that work presumes memory
corruption that may not exist.

**(1) PARTIALLY ANSWERED: the client is NOT round-trip-amplification-slow — it's blocking on
genuine multi-second dead stalls.** Directly measured: extracted every `diag-unix-stream`
timestamp after the client execve's (503 messages over ~37s). The gap distribution is NOT "many
round-trips each slightly slow" — most consecutive gaps are ~90 MICROSECONDS (fast, normal
socket traffic), interrupted by a handful of MULTI-SECOND dead gaps with ZERO logged activity of
ANY kind (no socket traffic, no epoll activity, no memory ops, no fork_verify activity) during
them:
```
21.92 -> 23.76  (1.83s)
24.44 -> 29.38  (4.94s)
30.00 -> 39.74  (9.74s)   <- checked directly, genuinely nothing logged in this window
39.94 -> 43.39  (3.45s)
43.51 -> 47.11  (3.60s)
48.30 -> 54.45  (6.15s)
54.59 -> 56.86  (2.28s)
```
This refutes the round-trip-amplification theory: if thousands of round-trips each cost ms
instead of µs, spacing would be roughly even throughout, not fast bursts separated by
multi-second silence. **The process is genuinely blocking on something** (a wait/poll/timeout, a
lock, a resource) during these gaps, not doing slow-but-steady protocol work.

**CONFIRMED FROM TWO INDEPENDENT ANGLES: the stall is a GLOBAL FREEZE, not the X client blocking
on something specific.** advisor-db checked the ENTIRE log (all processes, all subsystems) for
gaps and found the same multi-second dead windows with NOTHING logged by ANY process —
weston, Xwayland, the shell, no memory ops, no fork activity, all silent simultaneously
(`t=3.71` gap 4.23s, `t=8.54` gap 1.08s, `t=10.28` gap 6.81s, `t=21.31` gap 5.16s — 17.3s of dead
time in a 98s run). Independently, this session's own `sys_ppoll`-scoped debug capture confirms
it from a different subsystem: Xwayland's own event loop (`tid=12`, which normally spins at
~5-7ms poll intervals continuously) ALSO goes completely silent for the exact same window
(t≈29.44 to t=39.36 in that run) — Xwayland is not doing anything either, not just the client.
**A single guest thread waiting on a timer would not silence weston, Xwayland, and the shell all
at once — every guest thread stops together.**

**Leading theory: lock contention, likely `VIRTUAL_PROTECT_LOCK`/`ALLOCATE_PAGES_FIXED_ADDR_LOCK`
(the same lock, two names) held across a large `fork_duplicate` copy.** This session already
landed a fix (`984927b0`) making `unmap_shared_memory` take this same shared lock, and it's
already known to be shared across allocate/deallocate/protect paths. If `PageManager::duplicate()`
holds this lock for the DURATION of copying a large region (confirmed elsewhere in this
investigation: `fork_duplicate` copies up to 110,206,976 bytes, and 197 `fork_duplicate`
operations were observed inside one single 450ms window), every OTHER guest thread that touches
memory during that copy blocks behind it — producing exactly the observed "everything freezes at
once" signature. Fits the earlier (now-recontextualized) dose-response finding: more concurrent
forking correlates with more/longer freezes.

**CAUTION before acting on this**: a harness's own `sleep 0.5` polling loop in a launch script
produces regular ~0.5s "gaps" that are NOT real stalls — exclude those; only the irregular
multi-second gaps are the real signal. Also: **host memory pressure is a live, real confound
right now** (multiple concurrent sessions/agents running heavy launches) — before concluding this
is a genuine litebox lock-contention bug, re-run the fast repro with real memory headroom and
check whether the stalls shrink or vanish, to separate "real litebox bug" from "tonight's
memory-pressure-induced host scheduling noise." This distinction should be settled BEFORE
changing any locking code.

**CONTROL TEST DONE: memory pressure is EXCLUDED, and the result confirms the lock-contention
theory decisively.** Same repro, same binary, same script, two host memory states:
```
2.6 GB free:  4 stalls, 17.3s stalled of a 98s run   (18% stalled)
9.3 GB free:  5 stalls, 106.1s stalled of a 117s run (91% stalled)   -- includes a single
                                                                          30.8s stall AND a
                                                                          single 59.8s stall
```
**With nearly 4x the free memory, stalls got dramatically WORSE, not better.** This directly
excludes "tonight's host memory pressure/concurrent-session noise" as the explanation — the
stalls are reproducible and severe regardless of host state, confirmed on the SAME code both
times. This also confirms the lock-contention mechanism predicts EVERY observed property:
- all guest threads silent simultaneously → a global lock, not a per-thread wait
- duration varies wildly (1s to 60s) → scales with the size of whatever holds the lock
- **worse with MORE free memory** → larger copies SUCCEED and run to completion (holding the
  lock the whole time) instead of failing/bailing early when memory is tight — this is the
  counterintuitive result that most sharply confirms the theory over any host-noise explanation
- correlates with fork activity → `fork_duplicate`'s eager copy is the big lock-holder
- dose-response with concurrency (measured much earlier this session) → more concurrent forks,
  more contention

**LOCK-CONTENTION THEORY REFUTED BY DIRECT MEASUREMENT — do NOT touch `VIRTUAL_PROTECT_LOCK`/
`ALLOCATE_PAGES_FIXED_ADDR_LOCK`'s scope, that was a false lead.** The reasoning above (memory
headroom correlating with hold duration) was plausible but wrong -- exactly the class of error
this investigation has repeatedly had to catch via measurement rather than inference. Decisive
test: instrumented the lock at `lib.rs:5425` with both WAIT-to-acquire and HOLD duration timing,
logging any acquisition where either exceeded 50ms. Same 16s repro (weston + Xwayland + one X
client), 3 stalls observed (4.27s, 5.38s, 5.22s):
```
lock acquisitions held >= 50ms:        ZERO
lock acquisitions that waited >= 50ms: ZERO
```
**Not one acquisition of this lock even reached 50 milliseconds** — neither a long hold nor a
long wait, anywhere in the run. If this lock were the mechanism, the holder would show a
multi-second HOLD and blocked threads would show multi-second WAITs; neither appears. The
structural code-reading analysis of what the lock covers was correct — it just isn't the cause of
these stalls. Narrowing or restructuring it would reintroduce the real TOCTOU race it exists to
prevent, for zero benefit.

**What still stands, measured not inferred**: stalls are global (every process/subsystem silent
at once, confirmed two independent ways); NOT host memory pressure (9.3GB free made it WORSE than
2.6GB); NOT lock contention (zero slow acquisitions, just refuted above); stalls recur even in
the minimal repro with very little forking (t=3.5, 9.8, 19.1 observed in one run).

**New leading candidates, ranked**:
1. **Something that deliberately suspends ALL guest threads simultaneously BY DESIGN** — `fork`'s
   `kill_other_threads` path, or any `fork_verify` single-step pass that suspends threads. A
   suspend-all that then waits on one thread which is itself slow to reach a safe point would
   produce exactly this signature (global freeze, no single lock implicated). **Top suspect.**
2. A host-side GC/allocator pause inside the runner process itself (the Rust host allocator, not
   litebox's guest-facing page management).
3. Waiting on a Windows synchronization object with a long/infinite timeout satisfied late.

**Next step, not yet done**: search for `kill_other_threads` and any thread-suspension code in
`litebox_shim_linux`/`litebox_platform_windows_userland` (likely in the fork/clone path and
possibly `fork_verify.rs`'s single-step machinery). Log entry/exit of any all-thread-suspend
operation with duration — if a suspend-all call's own duration spans a stall window, that names
the mechanism directly and points at a far more tractable fix than anything involving locking.

**A "60-second timeout" theory was proposed and then self-corrected within the same
investigation — recorded here so it isn't re-derived.** A striking measurement (successive
stall-end timestamps across 4 independent runs differing by exactly ~60.000s, sub-10ms alignment)
initially looked like a missed-wakeup-rescued-by-timeout bug. Follow-up showed this was a
misreading: at each 60-second boundary, the SAME epoll entry fires (`entry_id` fixed,
`events_bits=1` then `=0` ~90µs later), then 60s of total silence — a periodic HEARTBEAT the
guest itself set (in the ~208s run, only SIX log events total occur after t=30), not a rescued
waiter. Static grep for `60_000`/`60000`/`Duration::from_secs(60)` across the tree also found
nothing, consistent with this being a guest-side timer, not a litebox one. **Do not chase a
missed-wakeup-on-a-60s-timeout theory** — it's refuted.

**CURRENT BEST UNDERSTANDING, replacing the timeout theory: a client permanently stalls, not
periodically.** `xfce4-about --version` never exits across a 208-second run — does real work for
~25 seconds, then goes PERMANENTLY quiet (not throttled, not periodically slow — simply stops
making progress at all, forever, except for its own unrelated heartbeat timer described above).
This is the signature of a client **waiting for a reply that never arrives** — most likely a
protocol response from Xwayland that's owed but never sent.

**FOUND — the full coherent picture, likely the actual root cause.** At t=28.03, in one 100ms
window, everything observed together:
```
client creates shared memory handle=592, size=245,760, maps it, VirtualProtects it
client sends 112 bytes to the compositor (a surface commit)
weston page-flips fb_id=2, handle=516, size=8,294,400
that scanout reads nonzero_bytes=0
```
`245,760 = 320*192*4` is a small CLIENT SURFACE buffer. `8,294,400 = 1920*1080*4` is the SCANOUT
buffer. **The client IS allocating a buffer, IS drawing, and IS committing it to the compositor.
weston IS receiving the commit and IS page-flipping. But the client's surface never appears in
the scanout, which stays at exactly zero.**

**This makes it a COMPOSITING problem, not memory corruption.** Nothing wipes the scanout buffer
— weston composites an EMPTY SCENE into it and flips that (correctly, mechanically). This
retroactively explains every observation this whole investigation collected: no decommit/unmap/
commit ever touches the scanout (0 overlaps in 19,133 events) because nothing does; the buffer
ends "strictly emptier than initial state" because weston clears to transparent black (alpha
included) vs. the dumb-buffer allocation's opaque-black initial state; fb→handle mapping stable
because it was never the problem; weston stops flipping afterward because an unchanging empty
scene generates no damage; the client never exits because it's waiting for a frame callback that
never comes, since its surface isn't being composited.

**Where to investigate now — a genuinely different area from everything dug into so far — why
weston does not include the client's surface in its scene**, in priority order:
1. The surface is never "mapped" — the commit arrives but weston doesn't treat it as ready to
   show (missing/mis-handled `wl_surface.attach`/`commit` sequence, or weston rejects the buffer).
2. **The shm pool import fails silently on weston's side** — weston has a surface with no usable
   buffer content. Closest to litebox's own code (weston maps the client's 245,760-byte pool
   through the shim's shared-memory path) — and this project already found ONE shm bug this
   session (the memfd mmap-time wipe, already fixed). If weston's mapping of the CLIENT buffer
   reads as zero, the client draws into one view while weston reads a different one — a real
   litebox bug, on the client-buffer path instead of the scanout path.
3. Xwayland's rootful window isn't being given a shell surface role at all, so weston has nothing
   positioned to draw.

**Cheap decisive test, not yet done — same instrumentation already built, pointed at a different
handle**: sample the CLIENT's buffer (handle 592, 245,760 bytes) the same way the scanout buffer
is already sampled. If it's non-zero, the client drew successfully and weston is failing to
composite it (candidate 1 or 3). If it's zero, the client's own drawing isn't landing at all
(candidate 2, a shared-memory bug in the client-buffer path).

**CLIENT-BUFFER SAMPLING INSTRUMENTATION ALREADY EXISTS — confirmed by code reading, this session
(`litebox_platform_windows_userland/src/lib.rs`, `map_shared_memory`, ~line 6139-6161): every
`map_shared_memory` call already logs `nonzero_in_sample` (first 4KiB of any buffer <=4MiB),
gated the same as the scanout digest. The doc note above ("not yet done") is stale relative to
current code; some peer session already landed this. Re-run and grep `nonzero_in_sample` rather
than adding new instrumentation.**

**One re-run this session (`fast_repro.sh` inside `layer31_direct_fixed.tar`, plain
`xfce4-about --version`, `LITEBOX_DRM_TRACE=1`) did NOT reproduce advisor's t=28 compositing
picture at all — a different, earlier divergence, underscoring the session's already-documented
non-determinism:**
```
t=3.2-4.7   client creates several shm handles (4904/4908/4940, sizes up to 8,294,400)
t=4.50      map_shared_memory FAILED handle=4980 win32_err=1132 (ERROR_MAPPED_ALIGNMENT) x3,
            correctly retried/handled per the existing NoReplace/AddressInUse fallback -- not a bug
t=4.51-4.68 create_shared_memory handle=4988 size=245760 (matches the 320x192x4 client-surface
            shape advisor described) and handle=4992 size=4096 -- but NEITHER is ever mapped via
            map_shared_memory anywhere later in this run (zero nonzero_in_sample events at all)
t=5.06-5.79 scanout genuinely has real content, nonzero_bytes=6,221,881 (weston's own shell paint)
t=16.4-17.4 Xwayland starts; scanout wipes to nonzero_bytes=0 and stays there through TEST_DONE
t=21.1      execve xfce4-about; prints "xfce4-about 4.20.1 (Xfce 4.20)" and exits promptly;
            Xwayland logs "failed to read client connection (pid 20)"
t=38.8      a NEW create_shared_memory handle=4724 size=245760 appears (some other client/process)
final frames (LITEBOX_DUMP_FRAMES): non_black_pixels=0 -- standing goal NOT met in this run
```
This run's `xfce4-about --version` behaved like a normal short-lived CLI probe (prints version,
exits), not like advisor's characterized long-lived GTK client that draws a 320x192 surface and
stalls at t=25-28 waiting on a compositor reply. Both shapes are real and reproducible on
different runs -- **whether `xfce4-about --version` builds a real GTK window (and thus a wl_shm
surface) at all may itself be non-deterministic or environment-dependent** (frozen locale/DISPLAY
race, GTK falling back to a no-display code path, etc.) and is itself worth checking directly
(`ldd`/`strace`-equivalent on what `xfce4-about --version` does on real Linux) before spending
more time chasing the compositing theory on a client invocation that may not even reach the
drawing code path every time.

**Next step for whoever picks this up**: (1) confirm whether `xfce4-about --version` is expected
to create a GTK window on real Linux at all (if not, swap the repro's client for one that
definitely does, e.g. plain `xfce4-terminal` or a minimal wayland/X11 test client that always
maps a surface) so the repro reliably reaches the code path the compositing theory is about; (2)
once a client reliably reaches `map_shared_memory` for its own surface buffer, re-run with
`LITEBOX_DRM_TRACE=1` and read `nonzero_in_sample` directly off the existing instrumentation
(no new code needed) to settle candidates 1/2/3 above.

**(2) IS NOW DONE, on a run where the client DID reach the drawing path — DECISIVE, COMPOSITING
THEORY CONFIRMED. STOP ALL MEMORY-CORRUPTION WORK.** Sampling every shared mapping under 4MiB at
map time in the 16s repro:
```
handle=540, size=245,760: nonzero_in_sample=0 at map (t=2.51), then 1024 at t=2.64
handle=600, size=245,760: nonzero_in_sample=0 at map (t=25.65), then 1024 at t=31.65
```
245,760 bytes = 320*192*4, a client surface buffer. It starts empty then HAS CONTENT. Other
client buffers show the same pattern (36864→4096 nonzero, 20480→1664, 40960→1664). **The client
draws successfully, and litebox delivers that content faithfully through the shared-memory path
— the shared-memory path WORKS.** Meanwhile the 8,294,400-byte scanout stays at exactly zero
throughout the same run. **The client has pixels; weston is not compositing them into the
scanout. This is confirmed as a compositing problem, not memory corruption.**

**STOP, effective immediately, do not resume without strong new contrary evidence**:
`GetWriteWatch` on the scanout, any `PAGE_READONLY` write-trap, any further lock-scope work, any
further hunt for what "zeroes" the framebuffer. Nothing zeroes it — weston composites an empty
scene, and an empty scene reads as zero.

**Caveat on the above (advisor, precise on purpose — do not overread this)**: each client handle
above was seen mapped at TWO distinct addresses (e.g. handle=540 at addr=482,082,816 reading 0,
and addr=928,317,440 reading 1024). It is tempting to read this as "the client sees content,
weston's own mapping reads zero" — **it does not show that.** Both samples were taken at MAP
time, so the zero reading is just a mapping established before the client had drawn, and the
non-zero one is later — same object, two different moments, not necessarily two different
processes' views. (Also: every mapping logs host pid 25532 for all of them, because all guest
"processes" are threads in one shared host process, so pid cannot be used to distinguish
client-side vs weston-side mappings here.) The still-open, still-decisive test is: sample BOTH
the client's mapping and weston's own mapping of the SAME handle at the SAME instant. If weston's
reads zero while the client's reads non-zero at that instant, that's a genuine litebox
cross-process shared-mapping bug (new territory, never instrumented this session). If they agree,
litebox is delivering correctly and the bug is entirely inside weston's own scene graph (surface
role / damage / repaint scheduling) — likely not a litebox bug at all. Cheapest next discriminator
per advisor: weston's own debug flags (surface role, damage, repaint-scheduling logging) may name
the reason directly, without guessing from memory contents.

**Methodology finding (advisor, applies broadly — audit other launch scripts for this)**: a
background service (e.g. `weston ... > file 2>&1 &`) that dies with a clear fatal error prints
that error ONLY into the redirected file, never into the main log. The launcher then just times
out waiting for the socket/marker that service was supposed to create, which looks exactly like a
stall or a litebox bug rather than what it is (a bad CLI arg / fast crash). Confirmed directly: a
stray `n` typo on weston's command line caused `fatal: unhandled option: n` + immediate exit,
invisible until the redirect file was read by hand; the launcher reported `WESTON=60` (full
timeout) with zero indication in the main log of why. **Any script backgrounding a service with
`> file 2>&1` should either tee to the console too, or `cat` the file automatically on a
readiness-timeout path — silent redirects turned real, fast, self-explanatory crashes into
mysterious multi-minute "hangs" for a meaningful fraction of this session's wasted investigation
time.** `advisor/probes/run_xfce_staged.sh` and this session's own probes (`fast_repro.sh`,
`scanout_wipe_repro.sh`, `scanout_wipe_discriminator.sh`) all redirect weston/Xwayland/xfwm4/
xfsettingsd/xfdesktop/xfce4-panel this same way — treat as a liability, not a feature, and fix
before further debugging sessions burn time on phantom "timeouts."

**AUDITED AND FIXED (this pass) for every probe script that has real source on disk**:
`run_xfce_staged.sh` (added `xfconfd.out` to the existing unconditional end-of-run `cat` loop --
every other redirected service there was already covered), `scanout_wipe_repro.sh` (now
unconditionally `cat`s `xc.out`, the client's own output, alongside the already-fixed
`weston.out`), and `scanout_wipe_discriminator.sh` (previously had NO file redirects at all for
seatd/weston/Xwayland — fine for visibility but meant nothing to `cat` on a hang either; now
redirects all of them plus every client (`xc1`-`xc4`) to files and unconditionally `cat`s all
seven at the end). **`fast_repro.sh` could NOT be fixed the same way** — it exists only packed
inside `.wfgy/xfce-build/layer31_direct_fixed.tar` (confirmed via `tar tf`), with no source file
anywhere in this repo; whoever packed it did so ad hoc. If it's still in active use, extract it
from the tar, apply the same unconditional-`cat`-at-end fix, and repack — or replace it with
`run_xfce_staged.sh`/`scanout_wipe_repro.sh`, which are equivalent and now fixed.

With the weston-arg typo fixed, weston's own log confirms the DRM/wgpu emulation path is fully
healthy — no errors/warnings anywhere in weston's own startup: `weston 14.0.2`, OS reports as
`LiteBox, 5.11.0, x86_64`, `drm-backend` loads, libseat/seatd session granted, `/dev/dri/card0`
in use, `Using Pixman renderer, shadow framebuffer`, head `Virtual-1` connected at
`virtual-1920x1080@60.0`, `desktop-shell.so` loaded, input device associated with the output. This
is a genuinely good, previously-unconfirmed result for the DRM/wgpu work: weston itself considers
litebox's virtual display device fully functional. The corrected run reproduces the same frame
pattern (19 real frames, then zero) — the scanout-blackout timing/behavior is unchanged by this
fix, so it does not explain the blocker, but it does rule out "weston doesn't like the DRM device"
as a contributing theory.

**Where the actual bug is now, in priority order**:
1. **weston's own import of the client's `wl_shm` pool — closest to litebox, cheapest to test
   with existing instrumentation.** weston receives the commit and must map the client's
   245,760-byte pool on ITS OWN side. Compare: does the SAME handle (540 or 600 above) get mapped
   a SECOND time by weston's own process, and does THAT mapping's `nonzero_in_sample` agree with
   the client's? If weston's own view of the identical handle reads zero/empty while the client's
   view is non-zero, that is a cross-process shared-mapping consistency bug — genuinely litebox's,
   but on a completely different code path than the scanout (never investigated this session).
   If weston's view agrees (non-zero), litebox is faithfully delivering the content and the bug
   is entirely inside weston's own scene-graph handling — likely NOT a litebox bug at all.
2. Surface role and mapping — a `wl_surface` with a buffer attached but no assigned role, or
   never properly mapped, is legitimately not composited by a correct compositor. Xwayland's
   rootful window needs a shell-surface role from weston's desktop-shell.
3. Damage/frame-callback handling — the client waiting forever for a frame callback is consistent
   with weston never scheduling a repaint that includes it.

**If (1) comes back "weston's own view agrees, non-zero"**: this is very likely NOT a litebox bug
at all, and the standing goal may need reframing around a weston/Xwayland-side workaround (e.g. a
different shell/compositor configuration, or an upstream weston fix) rather than a litebox code
change — worth surfacing to the user as a real possible outcome, not assumed to always be
litebox's fault to fix.

**LIKELY ROOT CAUSE FOUND (web research), cheap to test, try BEFORE any more shared-memory
forensics: no XWM (X Window Manager) is running.** Our setup launches weston with
`--shell=desktop-shell.so` and spawns `Xwayland :1 ...` as a bare separate process. Rootful
Xwayland launched this way needs the launching compositor to also attach an X Window Manager over
a separate `-wm <fd>` connection — that XWM is what maps an X11 window's Wayland surface into the
compositor's scene graph on `MapNotify`. weston's `desktop-shell.so` implements `wl_shell`/
`xdg-shell` roles for NATIVE Wayland clients ONLY — it has no XWM logic. XWM support lives
exclusively in weston's OWN `xwayland` module (`xwayland.so`), which is loaded via
`[core] xwayland=true` in `weston.ini` and which spawns AND manages Xwayland itself (including
the `-wm` fd handshake). By manually spawning `Xwayland` as an unrelated separate process, we
bypass this entirely: **the client's wl_shm buffer gets written with real content (matches our
own instrumentation exactly) but the surface never receives a role/gets mapped into weston's
scene graph, because nothing ever performed the XWM's map-on-MapNotify step.** This is a known,
documented pattern (Arch Wiki Weston page, weston.ini man page both describe `xwayland=true` as
the supported mechanism; the separate "Xweston" project exists specifically to swap out
desktop-shell for an external WM, confirming XWM duties and the shell are coupled, not
independent).
**Fix to try next**: stop spawning `Xwayland` manually. Instead set `xwayland=true` under
`[core]` in a `weston.ini` weston can find, ensure `xwayland.so` is present/loadable in the layer
tar, let weston launch Xwayland itself, and point clients at the `$DISPLAY` weston exports (rather
than hardcoding `:1` and manually waiting for `/tmp/.X11-unix/X1`).
**Cheap diagnostic if the fix doesn't immediately work**: weston's `scene-graph` debug scope
(`--debug` + `--logger-scopes=scene-graph`, or live via the `weston-debug` protocol client)
dumps every layer/view/surface + buffer info on demand, without requiring the client to exit —
this would show directly whether the X11 client's surface has ANY view/layer entry in the scene
graph at all, confirming or refuting this theory in one shot.

**MEMORY PATH FULLY EXONERATED — litebox's shared-memory implementation is CORRECT.** advisor's
same-instant cross-view comparison now covers the actual surface pools (the 245,760-byte buffers
that carry window pixels, not just protocol/cursor-sized objects), and ALL 11 comparisons agree:
```
handle=540 views=[(482082816, 1024), (928317440, 1024)] agree=true
handle=108 views=[(929169408, 1024), (929562624, 1024)] agree=true
```
plus 4096/12288/20480/36864/40960-byte handles, all agree=true. Two genuinely different mappings
of the same surface pool, read at the same instant, contain byte-identical content. **litebox's
cross-process shared memory is correct end-to-end, including for the exact buffers that carry
window pixels — there is no memory-path bug anywhere in this story.** Combined with the earlier
XWM research this fully explains every measurement taken all session: client buffer has content
(measured) -> both processes see identical content (measured, 11/11 agree) -> scanout is exactly
zero (measured) -> weston's own log is clean/happy with the DRM path (measured) -> client never
finishes startup, waits forever (measured, consistent with a surface that's never mapped so its
frame callback never fires). **A surface with a valid buffer but no role, never entered into
weston's scene graph because no XWM ever ran, explains all of it at once and requires zero litebox
code changes.**

**FIX CONFIRMED. ROOT CAUSE WAS OUR LAUNCH CONFIGURATION, NOT LITEBOX. THE BLACKOUT IS GONE.**
advisor-db tested it directly: added `xwayland=true` under `[core]` in the layer's
`/etc/xdg/weston/weston.ini` (`xwayland.so` was already present at
`/usr/lib/libweston-14/xwayland.so`), stopped spawning `Xwayland` manually, let weston start and
manage it itself, and discovered whichever display socket weston actually created (it picked `:0`
on its own — our old hardcoded `:1` was ALSO wrong) instead of hardcoding one. weston's own log
now shows the piece that was always missing:
```
[18:10:38.764] Loading module '/usr/lib/libweston-14/xwayland.so'
[18:10:39.080] Registered plugin API 'weston_xwayland_v3' of size 32
[18:10:39.080] Registered plugin API 'weston_xwayland_surface_v2' of size 24
[18:10:50.253] launching '/usr/bin/Xwayland'
[18:11:05.429] created wm, root 98
```
`created wm, root 98` is the XWM attach that was never happening before. Result:
```
BEFORE (manual Xwayland spawn): 19-23 frames, then px=0 permanently, wipe at t=16-23
NOW (weston-managed Xwayland):  24 frames, run ENDS on px=2,073,597, NO WIPE AT ALL
```
First time all session the framebuffer still has real content at the end of a run. Combined with
the memory-path exoneration above (11/11 cross-view agree=true, including the surface pools),
this is a complete, closed explanation requiring **zero litebox code changes**: spawning Xwayland
as a bare separate process gives an X server with no window manager attached, so X11 client
surfaces get buffers with real pixels written into them but are never mapped into weston's scene
graph, so they never composite.

**NEXT STEP (in progress)**: run the full XFCE stack (not just one test client) this way — take
`advisor/probes/run_xfce_staged.sh`, remove its manual Xwayland launch, set `xwayland=true`, point
all XFCE components (xfconfd/xfwm4/xfsettingsd/xfdesktop/xfce4-panel) at weston's own display
instead of a hardcoded `:1`. Since every XFCE component already launches and stays alive (per
earlier findings in this doc) and the compositing path is now confirmed working, this is expected
to be the run that finally satisfies the standing success oracle (`non_black_pixels > 0` in the
FINAL frames of a full XFCE session, not just one test client).

**Confound found mid-verification, correctly NOT conflated with the XWM fix**: the first
full-stack attempt with `xwayland=true` progressed cleanly (DBUS/SEATD/WESTON/XWAYLAND/XFCONFD/
XCHECK stages, 18 frames at 2,073,597 non-black, no wipe at t=36) but hit `DBUS_UP=no` — the
long-known intermittent trampoline `#UD` (`Exception(6) rip=0x7feffff7fb8a`, same bit-identical
address as every prior occurrence this session) fired again and killed the backgrounded
`dbus-daemon` before it could exec (`dbus-daemon execs: 0`), so `xfconfd`/`xfsettingsd`/
`xfdesktop`/`xfce4-panel` can't start (would fail with "Connection refused" as in every earlier
run). **This is a SEPARATE, already-known-intermittent bug (partially mitigated by b4330590,
still not fully closed) — it does NOT retroactively implicate or exonerate the XWM fix either
way.** A run where an unrelated component fails to start is not a fair test of the compositing
fix in either direction: non-black final frames from such a run wouldn't prove the XWM fix handles
a full desktop, and black final frames wouldn't disprove it either, since half the desktop never
launched. Correct discipline (applied): rerun until a clean `DBUS_UP=yes` run is obtained, and
only then read the final-frame oracle. **The isolated (non-full-stack) result is unaffected by
this confound and stands on its own regardless of how the full-stack run lands**: weston managing
Xwayland itself (`xwayland=true`) ends a run at 2,073,597 non-black pixels with no wipe, vs.
permanent blackout before — reproducible, doesn't touch dbus at all. Anyone hitting `DBUS_UP=no`
in a future full-stack run should treat it as this known trampoline `#UD` flakiness, retry, and
not read anything into the frame result of that specific run.

**FULL-STACK RESULT: COMPOSITING FIX VERIFIED END-TO-END, ORACLE PASSES — with one honest,
correctly-flagged caveat.** advisor-db's full-stack run with weston managing Xwayland:
```
23 frames captured; last four (t=24.40, 24.44, 37.49, 37.53) ALL non_black_pixels=2,073,597
no zero frame anywhere in the run -- the first full-stack run all session that never blacks out
alive at end (t=75.5): seatd, weston, xfwm4, xfdesktop
```
Every previous full-stack run went to zero and stayed there; this one holds real content to the
end. **The compositing/blackout blocker that dominated this entire session is fixed and verified.**

**Caveat, stated precisely and NOT to be glossed over**: `dbus` was lost to the same trampoline
`#UD` again in this run (`DBUS_UP=no`, `Exception(6)` at `rip=0x7feffff7fb8a`, `dbus-daemon`
never execs), so `xfconfd` never started and `xfsettingsd`/`xfce4-panel` never came up. **This is
a PARTIAL desktop — window manager (xfwm4) and desktop (xfdesktop) running and rendering; panel
and settings daemon missing for the unrelated, already-known dbus `#UD` reason.** Do not call this
"XFCE working" until a run has both the compositing fix AND a clean dbus start (all of xfconfd/
xfwm4/xfsettingsd/xfdesktop/xfce4-panel alive) with non-black final frames — that is the actual
remaining bar for the standing goal, now narrowed to exactly one known bug.

**Durable fix, to be landed permanently in the launch scripts** (three fix + two hard-won
operational lessons):
1. `weston.ini` needs `xwayland=true` under `[core]`.
2. Do NOT spawn `Xwayland` manually — let weston launch/manage it.
3. Discover weston's own display rather than hardcoding `:1` (it has chosen `:0` in every run).
4. Capture backgrounded services' stderr AND print/tee it — silent redirects turn fast, clear
   crashes into mysterious multi-minute "timeouts" (see the earlier weston-typo methodology
   finding above).
5. The dbus spawn needs a bounded retry structured as a shell **function called N times**, NOT a
   `while`-loop body backgrounding inside the loop — advisor found that backgrounding from inside
   a `while` loop converts what should be a probabilistic single-child death into a deterministic
   death of the launcher shell itself (bit-identical registers across runs); a function call
   avoids this shape. advisor is testing this retry now; report pending.

**CORRECTION — item 5 above is WRONG, do not implement it. RETRY MAKES THIS WORSE, NOT BETTER.**
advisor tested the bounded dbus retry two ways (while-loop body, and a shell function called
three times) and BOTH kill the launcher shell itself, not just the dbus child:
```
attempt 1 (while-loop retry):        dbus child dies (Exception 6, rip=0x7feffff7fb8a),
                                      then the LAUNCHER SHELL dies at rip=0x7feffff6fb11
attempt 2 (function called 3x):      identical outcome, same two addresses
```
So the trigger is NOT loop-vs-function syntax (that was a reasonable but incorrect earlier
hypothesis) — it's **retrying a backgrounded spawn at all, after one child has already been
lost to this bug, that takes down the shell issuing the retry.** Consequence: **single-spawn is
the only currently-safe pattern.** If dbus is lost to the `#UD`, the correct response is to
**rerun the whole script, not retry in-script** — an in-script retry converts an intermittent
*partial* failure (missing panel/settings this run) into a reliable *total* failure (dead
launcher, nothing comes up at all). **Do not add a dbus retry loop to `run_xfce_staged.sh` or any
other durable script.** The trampoline `#UD` at `rip=0x7feffff7fb8a` (partially mitigated by
b4330590, still not fully closed) is now the single remaining bug standing between this session
and a reproducible complete desktop — worth prioritizing over any further cosmetic script work.

**SELF-CORRECTION (advisor) to the two entries directly above — read this before acting on
either.** The repeated dbus failures across tonight's full-stack runs were advisor's own fault,
not an intermittent litebox bug: the script that added the XWM fix carried over `set -x` from an
older script, and `set -x` is the EXACT trigger this session bisected hours earlier as
deterministically killing the first backgrounded child via the trampoline `#UD` (see the `set -x`
entry under "Useful techniques"/observer-effect notes elsewhere in this doc). Evidence: dbus-daemon
started perfectly in isolation on the current build, 3/3 runs, zero faults; it failed 4/4 inside
the full script that had `set -x`. **Correction to what stands from the two entries above:**
1. "Retrying kills the launcher shell" is still literally true as a measurement (bit-identical
   crash addresses, loop and function form alike) — but the retry was never actually needed. The
   correct fix was removing `set -x`, not working around its consequences. Do not read "retries
   are unsafe" in isolation without this context.
2. "The `#UD` intermittently kills dbus, full-stack verification stays flaky" is **overstated**.
   On the current build, with no shell tracing anywhere in the launch path, dbus is reliable. The
   `#UD` is real (still worth closing eventually) but is **not currently blocking full-stack
   verification** — remove `set -x` and it goes away for this purpose.

**Durable guidance that actually stands**: **never use `set -x` in any of these launch scripts —
use explicit `echo` markers at stage boundaries instead.** `set -x` is easy to reintroduce by
copying an older script (exactly what happened here) — anyone touching a launch script (including
the agent landing the durable fix) must grep for and remove `set -x` from every script they touch.
advisor is rerunning the full stack now with tracing removed — this should finally be a fair,
unconfounded test of the complete desktop; result pending. The compositing fix and its
verification are unaffected either way: both the isolated repro and the earlier (traced, dbus-
broken) full-stack run ended on 2,073,597 non-black pixels, and neither depended on dbus.

**IN PROGRESS, LOOKS LIKE THE FIRST FULLY UNCONFOUNDED RUN — NOT YET CONFIRMED CLEAN, result
pending.** With `set -x` removed AND the XWM fix in place simultaneously (first time both
conditions held at once):
```
DBUS_UP=yes
XFCONF_PROBE_RC=0        -- xfconf-query reached the daemon, settings available
XFCE_DISPLAY=:0          -- weston's own Xwayland, discovered not hardcoded
frames: 19 x 2,073,597, no wipe
running so far: xfconfd, xfwm4, xfsettingsd, xfdesktop
```
Every component that previously failed with "Connection refused" now has a working bus and
settings daemon — that whole failure class appears gone. **Do not treat this as confirmed
success yet**: advisor explicitly flagged two other faults in this same run (not dbus-related,
not yet identified) and will not call the run clean until those are checked. Final frame data and
surviving-component list pending.

**Durable configuration for `run_xfce_staged.sh`, now believed complete (6 items)**:
1. `weston.ini`: `[core] xwayland=true`
2. No manual `Xwayland` launch — weston manages it.
3. Discover the display weston chooses (currently `:0`) rather than hardcoding.
4. **No `set -x` anywhere** in the script or anything it sources — explicit `echo` markers at
   stage boundaries instead. (This one has bitten the project twice now — once in original
   bisection, once when advisor reintroduced it by copying an old script an hour ago — deserves an
   explicit check/grep in whatever lands, not just a comment.)
5. Single dbus spawn, no retry (retry-after-loss kills the launcher shell itself).
6. Capture backgrounded services' stderr AND print/tee it, so failures never present as silent
   timeouts.

## Reproduction commands

Full XFCE launch — **use `advisor/probes/run_xfce_staged.sh` as the launch script, NOT any
`set -x`-instrumented script** (`xfce_direct.sh`, if it still has `set -x`, will trigger the
still-open #UD bug above and derail the run before it ever reaches the rendering blocker):
```
cd C:\dev\litebox-main
cargo build --release -p litebox_runner_linux_on_windows_userland --target x86_64-pc-windows-gnu
export LITEBOX_LOG=error
export MSYS2_ARG_CONV_EXCL="*"
export LITEBOX_DUMP_FRAMES=1
timeout 100 target/x86_64-pc-windows-gnu/release/litebox_runner_linux_on_windows_userland.exe \
  --initial-files .wfgy/xfce-build/layer31_direct_fixed.tar \
  --gui \
  -- /bin/sh advisor/probes/run_xfce_staged.sh \
  > /tmp/pass_repro.log 2>&1
```
(`layer31_direct_fixed.tar` = `alpine-pinned2.tar` + `layer31_direct.tar` merged, soname-repaired.
If missing, rebuild: extract both into one directory, run
`python advisor/probes/fix_layer_sonames.py <dir>`, then `tar cf layer31_direct_fixed.tar -C <dir> .`)

Bare-rootfs fork-bug repro (fast, no display stack): see the regression oracle above.

## Useful techniques discovered this session

- **A stuck/hung litebox run holds the runner binary's exe file open, and `cargo build` then
  fails with "Access is denied" on that exe.** This looks like a toolchain problem but isn't —
  find and kill the leftover `litebox_runner*` process (Task Manager or
  `Get-Process litebox_runner* | Stop-Process -Force`) and the build unblocks immediately. Check
  for this FIRST before investigating any other cause of a build failing only with a Windows
  file-locking error.
- **Guest stdout is interleaved character-wise with litebox's own log lines** in the runner's
  captured output, making a crashing guest program's own diagnostic messages unreadable via
  normal line-based grep. Recovery: `sed 's/\x1b\[[0-9;]*m//g' run.log | tr -d '\n' | grep -oE
  ".{N}PATTERN.{M}"`. A proper fix (`LITEBOX_GUEST_STDOUT_FILE`, routing guest pty output to its
  own file) exists but currently only activates under `--pty-mode`, which itself has a bug — it
  breaks the top-level shell with SIGPIPE in a non-interactive harness context. Worth fixing
  properly if picked up again; in the meantime, redirect each guest process's own stderr to a
  file inside the launch script (`cmd > /tmp/cmd.log 2>&1 &`) and `cat` it — this is what
  actually recovered real error messages (e.g. "xfsettingsd: Could not connect: Connection
  refused") this session.
- **`Exception(N)`/`error_code` decoding**: `cr2=0x0` and `error_code=0x0` together rule out a
  page fault (both are always set for `#PF`) — that combination means `#UD` (invalid opcode), an
  instruction that decoded to invalid bytes, not a jump to unmapped memory. `rip==cr2` with a
  nonzero `error_code` whose low bit is set (e.g. `0x6`) means an instruction-FETCH fault on a
  not-present page — a genuine missing/unbacked mapping.
- **A stale non-socket file at a well-known path blocks the real daemon from starting.** A
  resumed writable layer can leave e.g. `/run/seatd.sock` as a regular file instead of the real
  socket from a prior run, causing `seatd -l debug &` to silently refuse to start
  ("Non-socket file found at socket path... refusing to start"). Always `rm -f` well-known socket
  paths at the top of a launch script when resuming from a prior writable layer.
- **A missing `--resume-from`/`--initial-files` file panics the runner with a stack overflow**
  instead of a clean error (`lib.rs:338` and `lib.rs:679`, both `.unwrap()` on a file-open
  result) — if you see "thread 'main' has overflowed its stack" right after an "os error 2" (file
  not found) message, the actual bug is your invocation, not litebox itself. Worth fixing these
  two `.unwrap()`s to a graceful error + exit if touched again.
- **The program path passed to the runner must be RELATIVE, no leading slash** (`bin/sh`, not
  `/bin/sh`) — a leading slash also hits the same ENOENT-then-stack-overflow panic shape above.

## Host memory hygiene — check before trusting any run's result

When multiple sessions/agents run heavy full-XFCE launches concurrently, host free memory can
drop low enough (confirmed: ~5 GiB free out of 16 GiB, one run outright died mid-launch with
`memory allocation of 1342177280 bytes failed` at t=11.8s, well before the run's real content —
e.g. the scanout blackout at t~19-20s — was ever reached) to silently corrupt oracle results.

**The dangerous failure mode**: a run that dies early looks like "no bug occurred" to any oracle
that only checks final frames or exit status — a FALSE PASS. A run genuinely ending on non-black
frames because it crashed at t=8s, before ever reaching a bug that only manifests at t=19s, is
indistinguishable from a real fix without checking the run actually completed its full intended
duration.

**Before trusting any launch-run result** (a regression-oracle count, a "the bug is fixed" claim,
a clean/passing frame capture): check host memory first
(`powershell -Command "Get-CimInstance Win32_OperatingSystem | Select-Object
FreePhysicalMemory,TotalVisibleMemorySize"`), and verify the run's own log shows it actually ran
for its full intended duration rather than dying early (check for the expected final log lines —
`TEST_DONE`, the expected number of frames, no unexpected `thread panicked`/allocation-failure
lines). **Prefer serializing heavy full-XFCE-launch runs across concurrent sessions rather than
running several in parallel** — wall-clock is the thing being optimized, and a false result from
memory pressure costs more total time than the parallelism saves.

## Disk hygiene — read before generating any new layer tar or crash dump

`.wfgy/xfce-build/` and `.wfgy/crash-dumps/` accumulated to **73 GiB** and **19 GiB**
respectively by 2026-09-03, mostly near-duplicate incremental layer tars from iterative
debugging (`layer77-idletest7.tar` through `layer84-wterm.tar` alone: ~5.5 GiB of snapshots that
were never cleaned up) and old crash `.dmp` files (~3.8 GiB each) from a session whose findings
were already fully captured as text in `AGENTS.md`/project memory. Total repo directory size hit
85+ GiB before cleanup. Cleaned up same day: `.wfgy/xfce-build/` now holds only the 4 tars
actually referenced by this file's "Reproduction commands" section
(`alpine-pinned2.tar`, `xfce-layer31-nopanel.tar`, `layer31_direct.tar`,
`layer31_direct_fixed.tar`, ~4.2 GiB total); `.wfgy/crash-dumps/` and `.wfgy/gdb-session/` were
deleted entirely (their findings are already written up as text — the dumps themselves added no
further value once analyzed).

**To prevent this recurring:**
- **A layer tar you build for one debugging iteration is disposable once you've extracted what
  you needed from the run.** Do not accumulate `layerNN-<description>.tar` snapshots — if you
  need to preserve a specific known-good state, name it something durable (e.g.
  `layer31_direct_fixed.tar`, matching what's actually referenced in this file) and overwrite it
  in place rather than incrementing a number and keeping every prior version.
- **A `.dmp` crash dump is disposable once you've extracted the fault address/module/stack you
  needed via `VirtualQuery`/`objdump`/gdb and written the finding into `AGENTS.md` or project
  memory as text.** Delete it after use — a full-process minidump is typically 3-4 GiB on this
  project, and the actual signal you need from it is a few lines of text.
- **Only tars actually referenced in this file's "Reproduction commands" section (or an active,
  in-progress investigation) belong in `.wfgy/xfce-build/`.** Before adding a new one, check
  whether an existing tar can be reused/overwritten instead of creating another numbered variant.
- **Periodically (or before ending a long debugging session), run `du -h --max-depth=1 .wfgy`**
  and clean up anything not currently referenced — this is now a known failure mode for this
  project specifically, not a one-off.
- **`rm -rf` on a directory another process (a running litebox instance, another session) has
  open fails silently with "Device or resource busy" on Windows, and a subsequent `mv <src>
  <same-name>` then lands INSIDE the still-existing target instead of replacing it** (confirmed
  live: a cleanup pass this session moved 4 essential tars into
  `.wfgy/xfce-build/xfce-build-keep/*.tar` instead of `.wfgy/xfce-build/*.tar`, one level deeper
  than intended, when the original `xfce-build/` directory couldn't be removed because a
  concurrent litebox process still had it open). Nothing was lost, but a peer session relying on
  the expected path got a false "file is gone" read. **After any `rm -rf`/`mv` cleanup pass on a
  shared directory, verify with `ls`/`find` that the result actually landed where you expect** —
  don't assume a `mv` to a name that used to be occupied succeeded as a plain rename.
