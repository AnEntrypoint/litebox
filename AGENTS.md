# litebox — current state (2026-09-03)

This file is the authoritative, up-to-date picture of what works, what's broken, and what to do
next. It replaces the previous pass-by-pass chronological log, which accumulated thousands of
lines of retracted hypotheses alongside real findings. The full prior history (every pass,
including every dead end) is preserved at `docs/AGENTS_ARCHIVE_2026-09-03.md` for anyone who
needs the detailed forensic trail — but start here, not there.

## Standing goal

Get XFCE actually rendering and staying up under litebox on a Windows host (no WSL, no
hypervisor — see `feedback_no_wsl_or_hypervisor` in project memory). **Not yet fully met, but
close: every XFCE process now launches and stays alive — the sole remaining gap is that the
framebuffer goes black shortly after Xwayland starts and never recovers, independent of XFCE
itself.** See "Rendering/scanout blocker" below for the precise remaining gap and next step.

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

**Reframed conclusion**: a live, correctly-shared 8.29MB section loses its contents to exactly
zero, during fork activity by a DIFFERENT process than the one that owns the mapping, with no DRM
operation involved and no destruction of the object.

**Decisive check, not yet done — this is a Windows platform-level lifetime question**: log every
`VirtualFree`/`MEM_DECOMMIT`/`MEM_RESET`/`UnmapViewOfFileEx` call with its address range, then
check whether any of them covers the framebuffer's mapped address during the wipe window
(t=20.8-22.1). Exactly-zero contents on a section that was never destroyed is the classic
signature of pages being decommitted and recommitted — decommit is the one operation that
produces this without touching DRM state at all. Working theory: a fork-path teardown or
relocation-walk touches a range that happens to include the shared scanout mapping, even though
that mapping belongs to a DIFFERENT process than the one forking.

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
