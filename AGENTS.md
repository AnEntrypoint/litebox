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

**Single highest-priority item: forked children die (SIGSEGV/SIGILL, `rip==cr2`) before they can
`execve()`, under concurrent forking only.** This is the one thing standing between the current
state and the standing goal. **The earlier "MAXCONCURRENT fork_verify healing passes" theory
below is REFUTED as of the most recent measurement — read the correction at the end of this
section before acting on the rest.**

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
- **Next concrete step, not yet done**: instrument exactly what real Windows memory state
  (`VirtualQuery`) the faulting address shows AT THE MOMENT OF THE CRASH, and trace backward from
  there — this measurement was identified but not reached in the most recent session. Given the
  page is genuinely not-present per Windows itself (not a permissions issue, not a wrong-target
  jump), the likely area is whatever commits/reserves the child's memory during
  `PageManager::duplicate()`/`fork_duplicate` under concurrent execution — check for a race there
  distinct from the now-fixed proactive-fixup locking gap.

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
