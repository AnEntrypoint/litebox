# litebox — current state (2026-09-03)

This file is the authoritative, up-to-date picture of what works, what's broken, and what to do
next. It replaces the previous pass-by-pass chronological log, which accumulated thousands of
lines of retracted hypotheses alongside real findings. The full prior history (every pass,
including every dead end) is preserved at `docs/AGENTS_ARCHIVE_2026-09-03.md` for anyone who
needs the detailed forensic trail — but start here, not there.

## Standing goal

Get XFCE actually rendering and staying up under litebox on a Windows host (no WSL, no
hypervisor — see `feedback_no_wsl_or_hypervisor` in project memory). **Not yet met.** See
"Open blockers" below for the precise remaining gap.

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

**UPDATE (this session): the `rip==cr2`/`PAGE_READONLY`-not-`PAGE_EXECUTE_READ` concurrent-fork
bug described in this whole section is ROOT-CAUSED AND FIXED.** See "FIX LANDED" at the end of
this section for the exact mechanism, the fix, and its regression-oracle verification. **A
SEPARATE, still-open concurrent-fork crash signature was found blocking the standing goal under
the real, much-heavier XFCE launch load** — see "NEW: second, distinct crash signature under
XFCE launch" further down. Read both before picking this up again.

**Single highest-priority item (historical framing, now fixed — kept for context): forked
children die (SIGSEGV/SIGILL, `rip==cr2`) before they can `execve()`, under concurrent forking
only.** This is the one thing standing between the current state and the standing goal. **The
earlier "MAXCONCURRENT fork_verify healing passes" theory below is REFUTED as of the most recent
measurement — read the correction at the end of this section before acting on the rest.**

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
- **CORRECTION — the "MAXCONCURRENT fork_verify healing passes" correlation from earlier this
  session is REFUTED.** Setting `LITEBOX_FORKVERIFY_OFF=1` (disables fork_verify's reactive
  single-step/AV-path healing entirely, proactive fixup left on) reproduces the SAME fault rate
  as normal — proving that reactive healing machinery is NOT the dominant contributor to these
  faults, contrary to the strong-looking dose-response correlation measured earlier (that
  correlation was real but was not causal, or was confounded by something else that also scales
  with concurrency). Conversely, disabling the *proactive* fixup pass instead spikes faults to
  ~31/run (nearly every child) — confirming that pass does real, necessary work and is not itself
  spurious corruption.
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

## Reproduction commands

Full XFCE launch (once the fork bug above is fixed, use this to verify the standing goal):
```
cd C:\dev\litebox-main
cargo build --release -p litebox_runner_linux_on_windows_userland --target x86_64-pc-windows-gnu
export LITEBOX_LOG=error
export MSYS2_ARG_CONV_EXCL="*"
export LITEBOX_DUMP_FRAMES=1
timeout 100 target/x86_64-pc-windows-gnu/release/litebox_runner_linux_on_windows_userland.exe \
  --initial-files .wfgy/xfce-build/layer31_direct_fixed.tar \
  --gui \
  -- /bin/sh /xfce_direct.sh \
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
