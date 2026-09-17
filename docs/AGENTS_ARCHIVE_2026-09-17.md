# litebox archive — 2026-09-17

Trimmed out of `AGENTS.md` to keep it under the 30KB working-set budget. Read this for the trail;
`AGENTS.md` keeps only the current-state pointer.

## Shared-kernel-heap selective-routing correction -- session detail (attribution: lanmower)

Full evidence trail for the "Shared kernel heap -- SELECTIVE-ROUTING CORRECTION LANDED 2026-09-17"
entry in `AGENTS.md`. Starting point: Track B step 3 (`c08182d`) had routed EVERY host-heap
allocation (`SLAB_ALLOC`, `#[global_allocator]`) through one 8 GiB fixed-base shared section,
reasoning this carried `LiteBoxX`/`GlobalState`/`DefaultFS`/per-process fd tables into shared
memory "for free". Live-proven not viable: a real `webtop_stack.sh` boot under
`LITEBOX_PROCESS_FORK=1` (sharing gate on) hit `memory allocation of 181493744 bytes failed` 218
times before Xvfb/dbus even started -- the shared bump allocator has no reclaim, and an ordinary
one-shot allocation (the OCI rootfs-reconstruction buffer every plain exec makes) is
indistinguishable at that choke point from genuinely-must-be-shared kernel state.

**Code changes** (`litebox_platform_windows_userland/src/lib.rs`):
1. `WindowsUserland::alloc`/`free` (the `MemoryProvider` impl backing `SLAB_ALLOC`) reverted to the
   EXACT pre-`c08182d` mechanism: per-call `VirtualAlloc2(MEM_COMMIT|MEM_RESERVE)` constrained to
   `HOST_ALLOCATOR_REGION_MIN..` via `MEM_ADDRESS_REQUIREMENTS`, `free` via
   `VirtualFree(MEM_RELEASE)`. Diffed directly against `git show c08182d` to confirm byte-for-byte
   equivalence of the restored logic (`diag_alloc` offset math, alignment doubling, everything).
2. The old bump-allocation body of `WindowsUserland::alloc` was NOT deleted -- extracted verbatim
   into a new standalone function `shared_kernel_arena_alloc(layout) -> Option<(usize, usize)>`,
   deliberately never called from `GlobalAlloc`/`#[global_allocator]` context. Still uses the exact
   same atomic cross-process cursor (`shared_heap_cursor`, `SHARED_KERNEL_HEAP_CURSOR_OFFSET`) and
   fixed-base/fallback mapping (`init_shared_kernel_heap`) from the prior two sessions' work
   (`3e81e1d`/`3d661d2`), unmodified.
3. `SHARED_KERNEL_HEAP_SIZE` shrunk `8 * 1024 * 1024 * 1024` -> `64 * 1024 * 1024` (64 MiB) --
   this region no longer needs to hold bulk process-wide allocation traffic, only a small, bounded
   set of future kernel-singleton allocations.
4. Doc comments on `SLAB_ALLOC`, `SHARED_KERNEL_HEAP_BASE`, `SHARED_KERNEL_HEAP_SIZE` rewritten to
   describe the corrected design and point at this archive entry.

**Why `LiteBoxX`/`GlobalState` are not wired to `shared_kernel_arena_alloc` this session** -- real,
identified blockers, not an oversight:
- `Arc<T>`'s `ArcInner` layout is a private std implementation detail. `Arc::from_raw` over bytes
  manually placed by `ptr::write` into arbitrary memory is unsound -- it must originate from
  `Arc::new`/`Arc::into_raw`. Rust stable has no `allocator_api`/`Box::new_in`/`Arc::new_in`, so
  there is no sanctioned way to tell `Arc::new` to use a non-default allocator. The only sound
  mechanism is a hand-rolled `SharedArc<T>`-style wrapper: `shared_kernel_arena_alloc` a raw
  `{AtomicUsize strong; T value}` block, `ptr::write` the value in, manual `Clone`
  (`fetch_add(1)`)/`Drop` (`fetch_sub(1)`, `ptr::drop_in_place` on last release, deliberately never
  reclaiming the backing bytes -- matches this codebase's existing "free is dead in practice" bump
  allocator philosophy already documented on `WindowsUserland::free`).
- `LiteBox::new` (`litebox/src/litebox.rs:32`, constructs `Arc::new(LiteBoxX{platform,
  descriptors})`) and `LinuxShimBuilder::build` (`litebox_shim_linux/src/lib.rs:469`, constructs
  `Arc::new(GlobalState{...22 fields...})`) are platform-generic code, `Platform: ShimPlatform`
  (`RawSyncPrimitivesProvider` etc.), shared verbatim by every runner: Windows, Linux native
  (`litebox_runner_linux_userland`), macOS, optee, snp, lvbs. A `SharedArc<T>` wrapper usable there
  needs a NEW trait (parallel to `litebox::platform::RawMutexProvider`) with a real impl only for
  `WindowsUserland` and a default no-op (ordinary `Arc::new`) for every other platform, so this is
  real, scoped, cross-crate work -- not attempted this pass to avoid an unverified half-migration
  landing on stable.
- Both `LinuxShimBuilder::new(platform)` (`LiteBox::new` call site) and `.build()` (`GlobalState`
  call site) are invoked from `litebox_runner_linux_on_windows_userland/src/lib.rs` (~line 780-822,
  a second call site ~line 1277) -- already Windows-specific, already depends on
  `litebox_platform_windows_userland` directly. THIS is where a future session should wrap just
  those two calls with a scope guard once the `SharedArc<T>` trait exists, rather than touching the
  platform-generic crates' function signatures at all.
- Deeper, separate blocker even after the wrapper exists: today's design has EVERY process in a
  fork family (the original parent AND every `CreateProcess`-based "fork" child) call
  `LinuxShimBuilder::new().build()` unconditionally at its own startup, constructing its OWN fresh
  `GlobalState`/`LiteBoxX`. Placing that allocation in shared memory does not by itself give a
  child the PARENT's already-open pipes/futexes/AF_UNIX table -- each process still ends up with
  its own distinct object at its own distinct offset in the shared arena. Real content sharing of
  the *singleton* needs a create-vs-attach protocol (first process in the family creates it at a
  well-known shared offset; every later process in the family detects the existing instance and
  attaches to it instead of constructing a fresh one) that does not exist anywhere in this codebase
  yet. Recommend tracking this as its own PRD, separate from "give `shared_kernel_arena_alloc` a
  real caller".

**Live verification, in order**:
1. `cargo build --release -p litebox_platform_windows_userland` and
   `-p litebox_runner_linux_on_windows_userland`: both clean (only pre-existing/expected warnings:
   `shared_heap_cursor`/`shared_kernel_arena_alloc` now genuinely unused dead code until a follow-up
   session wires a caller -- intentional, not a bug).
2. Cheap repro (`AGENTS.md`'s own recipe) under `LITEBOX_PROCESS_FORK=1`,
   `docker.io/library/debian:stable-slim`, a 5-iteration bash loop ending in a trailing command to
   force a real `clone()`: exit 0, all 5 `iter_N` lines plus `done_exit_0` present.
3. Sentinel-style proof of the shrunk 64 MiB standalone arena, `LITEBOX_PROCESS_FORK=1` +
   `LITEBOX_DIAG_SHARED_HEAP_INHERIT=1` + `LITEBOX_DIAG_SHARED_HEAP_PROBE=1`: parent-side
   init+map+commit+sentinel-write succeeded at `base + 0x3FFF000` (= `base + 64MiB - 4KiB`,
   confirming the shrink took effect correctly), value `0xc0ffee00deadec5f` written cleanly, real
   `task-resume-probe` lines confirming a genuine cross-process fork occurred. Child-side
   `[shared_kernel_heap] INHERITED`/`OBSERVED` lines did NOT fire this run -- expected and harmless:
   since `WindowsUserland::alloc` no longer auto-triggers `init_shared_kernel_heap()` on a process's
   first allocation (that trigger was the OLD everything-shared design), nothing in a child calls
   `init_shared_kernel_heap()`/`shared_kernel_arena_alloc()` today, so the child-side half of the
   probe is genuinely dormant until a real caller exists (see "why not wired" above) -- not a
   regression, since the parent-side mechanics (the actual thing that changed this session) are
   confirmed correct at the new size.
4. **The real target test**: `.wfgy/webtop_stack.sh` (`docker.io/linuxserver/webtop:debian-xfce`,
   17 layers, one 736 MiB), flags `--gui=hidden -p 8080:3000 --env
   GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0 --oci-image
   docker.io/linuxserver/webtop:debian-xfce --resume-from .wfgy/webtop_seed.tar -- /bin/bash
   /webtop_stack.sh`, `LITEBOX_PROCESS_FORK=1` + `LITEBOX_LOG=warn,...fork_verify=error` as real
   host env vars (sharing gate left OFF, i.e. today's actual default). Ran 200+s with 8-9 real
   concurrent `litebox_runner_linux_on_windows_userland.exe` processes alive simultaneously
   (confirmed via `tasklist`), reached `NGINX_CONFIGURED` -> `NGINX_STARTED` ->
   `NGINX_SELFTEST_FAILED` (pre-existing, separately-tracked nginx issue, unrelated to this fix) ->
   `XVFB_FAILED`. **Zero** `memory allocation of ... failed` lines across a 10,864-line combined
   stderr log (was 218 before this fix, in the same boot script). Zero panic/FATAL/abort markers.
   Killed cleanly via `Stop-Process -Force` on the whole process tree once the known terminal state
   (`XVFB_FAILED`) was confirmed reached and stable (nginx supervisor retry-looping, as documented
   pre-existing behavior); host free RAM fully recovered to the pre-boot baseline
   (~5.8M KB free / 15.9M KB total) within seconds, zero leaked/orphaned processes.

**Net result**: the capacity/OOM regression this session was scoped to fix is definitively fixed
and live-proven fixed on the actual real-world repro that originally found it. `XVFB_FAILED` is
reached at the SAME point as the historical sharing-off baseline (not further, not less) --
confirming the fix restores exactly the pre-Track-B-step-3 behavior for ordinary allocations without
reintroducing the old problem Track B was trying to solve (genuine `LiteBoxX`/`GlobalState`
cross-process visibility), which remains open, scoped, and precisely documented above for a
follow-up session -- this session did not attempt it, to avoid landing an unverified partial
migration.

## Closed — do not re-attempt without a genuinely new approach (moved verbatim from AGENTS.md)

**There is no open host crash** — the `RtlpUnwindPrologue` crash earlier notes called "the one genuinely
open" one was `VEH_FRAME_STRIDE`: a 4096-byte per-level slice 168 bytes short of the two frames it must
cover, nested by `fork_verify`'s own AV-heal storm. Bisected live: 10/10 fatal before, 0/10 then 0/57
after (two follow-up commits; mechanism: archive). **`veh-frame-stride-has-no-overflow-guard` — CLOSED
2026-09-16**: `VehFrameCanaryGuard` (`litebox_platform_windows_userland/src/lib.rs`, right above
`vectored_exception_handler`) stamps a canary above the next nesting level's slice floor and checks it
on `Drop`, `RaiseFailFastException`ing on mismatch instead of silent corruption. `cargo build --release
-p litebox_platform_windows_userland` clean. **Live-reverified same day**: its first real
cross-process-fork workout (pipe-relay-sigpipe investigation) hit it immediately, 3/3 children --
guard worked (no silent corruption) but 8 KiB/cap-3 was too small for this workload. Fixed:
`VEH_FRAME_STRIDE` 8->16 KiB, `VEH_DEPTH_CAP` 3->1 (same 32 KiB total, redistributed -- depth never
exceeded 1 across 20,000+ single-step exceptions on 3 processes). 0/9 recurrences since.

**`dev_bench`/`litebox_runner_snp` Windows build failures — CLOSED 2026-09-16, root cause was NOT
libc/seccomp.** `dev_bench`'s new `reap_children` called `libc::wait4`/`rusage`/`WIFEXITED`/
`WEXITSTATUS` unconditionally — absent from `libc` on `windows-msvc`; fixed via `#[cfg(unix)]`/
`#[cfg(not(unix))]` split. `litebox_runner_snp` was already correctly excluded from the default Windows
build path (`#![no_std]` SNP-guest kernel image, own custom target + pinned nightly + `-Zbuild-std`,
real error is "unwinding panics are not supported without std") — root `Cargo.toml` now documents why,
next to the `litebox_runner_lvbs` precedent, so it isn't re-diagnosed.

**Windows CoW-mmap performance**: zero practical effect on tar-packed execs (`MapViewOfFile3` needs 64KiB
file-offset alignment; ELF `PT_LOAD` segments are only page-aligned, no exploitable slack).
**`LITEBOX_COW_MMAP` default-off is load-bearing** — the shipped flank fix recommits orphaned flanks as
zero-fill; opting in trades a loud SIGSEGV for silently zeroed symbol tables
(`docs/cow-mmap-fixed-address-design.md`).

**Input latency**: three real bugs fixed and verified live (sub-pixel remainders now accumulated
losslessly; two evdev reports per move now one `SYN_REPORT`; window now resizable with scaled deltas).
Present mode is Mailbox-preferred with Fifo fallback — any note calling it Fifo-only is stale.

**Presenter-split reintroduced the duplicate-`SYN_REPORT` bug, fixed and live-verified 2026-09-16**
(`Request::RelMotion{dx,dy}` added to `litebox_presenter_protocol`; one `relmotion 5 3` now produces
exactly one `SYN_REPORT`). Open PRD, both DIFFERENT/untouched: `mouse-motion-devicevent-needs-pixel-
calibration`, `linux-macos-userland-presentation-still-emits-two-syn-reports-per-move`. Full repro:
`docs/AGENTS_ARCHIVE_2026-09-16.md`. No framerate baseline exists (idle compositor = zero flips).

**The GUI protocol decision is settled**: DRM/KMS + wgpu, proven live with guest page-flip pixels in a
real host window. Not an open X11-vs-Wayland-vs-DRM question.

---

## Terminal-emulator shell crash — investigated live, real mechanism found; NOT a leaked fd, NOT simply "flip LITEBOX_PROCESS_FORK=1" (2026-09-17)

Task: find out why xfce4-terminal's shell child crashes/hangs with just a blinking cursor and no
prompt, testing the sharp hypothesis that a terminal->shell spawn is a textbook `beyond_stdio==0`
fork+exec that should already be safe (ADVISORY-002 §1.5) or trivially fixed by enabling
`LITEBOX_PROCESS_FORK=1`.

**Finding 1 — `LITEBOX_PROCESS_FORK=1` is NOT set in the standard boot recipe.** Confirmed by reading
`.wfgy/webtop_stack.sh` in full (no `LITEBOX_PROCESS_FORK` anywhere) and by inspecting the live
demo process's real command line (`Get-CimInstance Win32_Process`): only
`--env GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0` is passed. The script's own
comment asserts flatly "`LITEBOX_PROCESS_FORK` breaks Xvfb/dbus on this image" with no supporting
per-kind breakdown. Separately, `AGENTS.md`'s existing claim ("on a real `debian-xfce` boot the only
remaining blocking kind is `unix-socket` — 5 refused forks of 34, down from 34/34") has **no
corroborating evidence anywhere in the tracked repo** — not in `docs/track-b-fork-fix-progress.md`
(3982 lines, the primary investigation log, whose own last-dated entry, 2026-09-07, leaves Step 0
"INCONCLUSIVE... blocked on getting a clean high-memory run", never CONFIRMED), not in either
2026-09-16 archive. Treat that specific "5 refused of 34" number as unverified/stale until a session
reproduces it with a fresh log pointer.

**Finding 2 — live-tested the actual hypothesis, twice, and it fails, but not for the reason assumed.**
Built a minimal, disposable repro (no test files written — passed inline via `-c`, matching the
project's existing `advisor/probes/tcache_fork_repro.sh` shape) using the cached
`docker.io/linuxserver/webtop:debian-xfce` image (`.litebox-cache` already warm, `[cache] HIT` on all
17 layers):

```
# baseline (default thread-based fork):
litebox_runner ... --env GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0 \
  --oci-image docker.io/linuxserver/webtop:debian-xfce -- /bin/bash -c \
  'echo XORG_START; Xorg :0 -nolisten tcp -noreset -novtswitch -sharevts > /tmp/xorg.log 2>&1 & \
   sleep 3; echo XORG_UP; echo FORK_EXEC_TEST_START; \
   timeout 15 /bin/bash -c '\''echo INSIDE_SHELL_OK; id; sleep 1; echo SHELL_DONE'\''; \
   echo FORK_EXEC_EXIT=$?; echo REPRO_DONE'
```

Result (baseline, default fork): Xorg's own backgrounding fork hits `double free or corruption (out)`
and SIGABRT (comm still `bash` at the crash, i.e. very early post-fork/pre-exec) within ~4s, then the
SAME address space's next fork (`timeout`) also SIGABRTs with `Fatal error: glibc detected an invalid
stdio handle`. **This is the already-documented, still-unfixed "second glibc corruption class"** from
AGENTS.md's own selkies section ("a SECOND, different corruption signature under heavy fork load...
hitting bin paths the tunables deliberately leave enabled") — not a new bug, and not specific to
xfce4-terminal. It reproduces on the FIRST fork of a fresh boot with zero desktop/terminal involved.

Result (same repro, `LITEBOX_PROCESS_FORK=1` added, `LITEBOX_LOG` set as a real HOST env var — note
`--env LITEBOX_LOG=...` does NOT work, that only forwards into the GUEST environment, not the
runner's own `tracing_subscriber`): **identical crash signatures, byte-for-byte**, at the same points.

**Finding 3 — decisive, clean isolation: even a bare, fd-redirection-free fork+exec crashes under
`LITEBOX_PROCESS_FORK=1`, in the cross-process fork mechanism itself, not in the guest's fd table.**
Removed Xorg and all redirection entirely — the simplest possible xfce4-terminal-shaped repro:
```
/bin/bash -c 'echo OUTER_START; /bin/bash -c ''echo INNER_SHELL_OK; id; echo INNER_DONE''; echo OUTER_EXIT=$?'
```
With `LITEBOX_LOG=warn,litebox_shim_linux::syscalls::process=debug,litebox_platform_windows_userland=debug`
set as a real host env var, the debug trace shows, unambiguously:
```
clone: try_cross_process_fork entry tid=1
clone: cross-process fork() is eligible -- the child gets ...
clone: cross-process fork() copy plan tid=1 regions=112
do_clone: about to duplicate address space for fork()
clone: request registered child_tid=2 parent_pid=1
clone: spawned new task parent_tid=1 child_tid=2
Fatal error: glibc detected an invalid stdio handle
ERROR ... fatal signal: terminating task signal=Signal(6) pid=2
```
`beyond_stdio==0` is confirmed, the eligibility gate is correctly passed, a genuine separate Windows
process (`D==0`) is spawned successfully (`spawn_cross_process_fork_child`/`spawn_process_fork_child`
in `litebox_platform_windows_userland/src/process_fork.rs`) — and the child's OWN glibc, running for
real in that new process, detects an invalid stdio handle before ever printing `INNER_SHELL_OK` and
aborts. The string `"glibc detected an invalid stdio handle"` does not appear anywhere in litebox's own
source (confirmed via `codesearch`), so this is genuine guest-side glibc behavior reacting to a real,
litebox-caused invalid-handle condition in the cross-process child's own fd 0/1/2 wiring — a distinct,
previously-uncharacterized bug in the cross-process-fork mechanism's stdio setup, not the tcache/
safe-linking class `LITEBOX_PROCESS_FORK` exists to dodge, and not anything to do with xfce4-terminal's
own fd hygiene.

`process_fork.rs`'s `spawn_suspended` (~line 3388) has two back-to-back blocks that both set
`STARTF_USESTDHANDLES`/`GetStdHandle` on the non-pipe (`inherit_stdio`) path (~3461-3534, commented
"FIX (track-b investigation, confirmed live)", and ~3546-3569, doing nearly the same thing again) —
worth a future session's attention as a possible redundant-write source of the invalid handle, but
NOT confirmed as the root cause this pass; applying a speculative fix to this file without being able
to drive it to a fully verified `TCACHE_REPRO_CHILD_OK`-equivalent green run (a bar multiple full prior
sessions already failed to clear in this exact file — see `docs/track-b-fork-fix-progress.md`'s own
"Step 9: still NOT REACHED" verdicts) would be the "unverified, wrong-target fix" this project's own
standing discipline explicitly warns against. Recorded as a real, live, reproducible finding for a
future session with more budget to drive to a fix, not glossed over.

**Verdict on the assigned hypothesis**: REFUTED, with real evidence, on two counts — (1) it is not a
leaked fd / missing-`FD_CLOEXEC` bug in xfce4-terminal's own spawn shape, since a completely clean
`beyond_stdio==0` repro still fails; (2) enabling `LITEBOX_PROCESS_FORK=1` is not currently a viable
one-line fix, because the cross-process fork path has its own live, blocking, previously-undocumented
bug independent of the corruption class it exists to avoid. The default (thread-based) path
independently still hits the already-known, already-documented, NOT-yet-fixed "double free or
corruption (out)" second-corruption-class bug on ordinary fork-heavy guest activity (Xorg's own
startup, no terminal or desktop involved at all). **This genuinely needs further Track B work on both
fronts** — the still-open "second glibc corruption class" for the default path, and the newly-found
cross-process-fork stdio-handle bug for the opt-in path — not a config/env-var change, and not a new
fork-without-exec hop specific to xfce4-terminal.

No source files were modified this pass (the investigation concluded no code fix was safely
verifiable within scope — see above). No `LITEBOX_PROCESS_FORK=1` guest processes or extra runner
instances were left running; the pre-existing live-demo runner (PID 2012/13108, no
`LITEBOX_PROCESS_FORK` set) was killed at the start of this investigation (per the task's own
authorization) and was NOT restarted afterward — a future session/user wanting the live desktop demo
back up needs to re-run `.wfgy/webtop_stack.sh` fresh.

## Follow-up session, same day: both suspect bugs fixed+verified live, plus one new bug found

Task: fix the `spawn_suspended` `STARTF_USESTDHANDLES` bug flagged above. Read both blocks in full
first (~3388-3570 pre-fix). Real bug found in the SECOND block (~3546-3569, `if inherit_stdio &&
!want_stdout_pipe && !want_stdin_pipe`): it unconditionally wrote
`startup_info.hStdInput/hStdOutput/hStdError = GetStdHandle(...)` and forced
`dwFlags |= STARTF_USESTDHANDLES` / `inherit_handles = 1`, with NO null/`INVALID_HANDLE_VALUE`
check — unlike the FIRST block (~3461-3534), which only assigns a stream when
`!h.is_null() && h != INVALID_HANDLE_VALUE`. So whenever the parent's own `STD_INPUT_HANDLE` (or
stdout/stderr) is invalid/null — a real condition for a non-interactively-launched/redirected
runner process — the first block correctly left that field unset, and the second block then
clobbered it back to the invalid value anyway, handing the child a `STARTUPINFOW` naming a
genuinely invalid HANDLE as one of its standard streams. The first block also ran on
`!want_stdout_pipe` ALONE, ignoring `inherit_stdio` — so the diagnostic memory-copy-only probe
(pass 111/112, `inherit_stdio=false`) got the parent's stdio wired in despite explicitly asking not
to. Fix: one block, `else if inherit_stdio`, keeping the first block's per-handle validity guard as
the only place `STARTF_USESTDHANDLES`/`hStd*` are set; the true `else` (`!want_stdout_pipe &&
!inherit_stdio`) now correctly leaves the child's stdio fully unset again.

**Reproduction, before fixing**: ~10 live runs of the exact `OUTER_START; INNER_SHELL_OK; id;
INNER_DONE; OUTER_EXIT=$?` repro from the investigation above (`LITEBOX_PROCESS_FORK=1`, cached
`debian-xfce`, `LITEBOX_LOG` with `litebox_shim_linux::syscalls::process=debug,
litebox_platform_windows_userland=debug`): 7/10 clean, 0/10 hit the documented `Fatal error: glibc
detected an invalid stdio handle`, 3/10 hit a DIFFERENT, new failure:
`[shared_kernel_heap] FATAL CreateFileMappingW failed win32_err=0x5aa requested_size=0x200000000`
followed by `/bin/bash: line 1: 2 Killed .../bin/bash -c 'echo INNER_SHELL_OK; id; echo
INNER_DONE'`, `OUTER_EXIT=137`. `0x5aa` = `ERROR_NO_SYSTEM_RESOURCES`. Also tried the task's own
literal suggested shape (`bash -c 'echo A; bash -c "echo B"'`, single-quoted to dodge the
documented quoting gotcha): 5/5 runs printed both `A` and `B` with ZERO `clone:`/fork debug lines
at all — GNU bash tail-exec's the final command of a `-c` script (no subsequent statement needs
the shell to survive), so this exact shape never calls `fork()` under real bash semantics and is
not a usable repro; use the `OUTER_EXIT=$?`-suffixed shape instead, which forces a real fork.

**Root cause of the NEW bug**: `init_shared_kernel_heap` (`litebox_platform_windows_userland/src/
lib.rs`) creates its 8 GiB pagefile-backed section via `CreateFileMappingW(INVALID_HANDLE_VALUE,
NULL, PAGE_READWRITE, ...)` with no `SEC_RESERVE` flag — Windows defaults this to `SEC_COMMIT`,
which charges the FULL 8 GiB against system resources at section-CREATION time, not lazily on
first touch as the function's own doc comment claims. Every cross-process fork child is a
genuinely separate Windows process that independently runs this same function on its own first
allocation, WHILE the parent's own already-live 8 GiB section still holds its charge — live-checked
system counters at failure time (`Win32_PerfFormattedData_PerfOS_Memory.CommitLimit` ~32.7 GB,
committed ~10.5 GB, ~9 GB free physical RAM, ~14.6 GB free pagefile space) show tens of GB of
nominal headroom, consistent with transient kernel-pool/VAD contention from two ~8 GiB
`SEC_COMMIT` sections coexisting rather than genuine exhaustion — this is Track B step 3
(`windows-userland: fixed-base shared kernel heap`, commit `c08182d`) hitting exactly the gap its
own AGENTS.md entry disclosed: "no second process has actually mapped the shared section yet (this
pass is single-process only)". Fix applied: bounded retry on `CreateFileMappingW` (8 attempts,
10ms/attempt linear backoff, ≤280ms worst case) before the existing `abort()` — this project's own
"bounded retry, then surface" doctrine applied to a measured-transient OS resource condition, not a
memory-semantics rewrite. The doc-comment-promised proper fix (`SEC_RESERVE` + explicit
per-allocation `VirtualAlloc(..., MEM_COMMIT, ...)` on the bump allocator's hot path, matching the
"reserve now, commit on touch" pattern this same file already uses elsewhere for `copy_one_group`/
`try_allocate_cow_pages`) is a larger change to a hot path and was deliberately NOT attempted this
pass — `PRD shared-kernel-heap-eager-full-commit-not-lazy-reserve`.

**Verification after both fixes** (`cargo build --release -p litebox_platform_windows_userland -p
litebox_runner_linux_on_windows_userland`, clean): 24/24 consecutive live runs of the exact
`OUTER_EXIT=$?` repro completed cleanly (`INNER_SHELL_OK`, `uid=0(root)`, `INNER_DONE`,
`OUTER_EXIT=0` every time) — 0 heap-abort (was 3/10), 0 glibc-abort (was 0/10, unchanged — never
witnessed either before or after). Host memory stable across all ~40 total repro runs this pass
(`FreePhysicalMemory` ~8.7-9.0 GB of 15.6 GB throughout, no leaked `litebox_runner` processes at
any point, confirmed via `Get-Process` after each batch).

**PTY test** (step 5 of the task): `bash -c 'which script; script -qec "echo PTY_START; id; echo
PTY_DONE" /dev/null; echo SCRIPT_EXIT=$?'` under `LITEBOX_PROCESS_FORK=1` — `script` itself was
killed by `fatal signal: ... signal=Signal(13)` (SIGPIPE) ~6s in, with `n_orphans=1` at exit
(a forked child process DID survive, so a real fork happened) — neither `PTY_START` nor
`PTY_DONE` were ever printed. This is a DIFFERENT bug from the stdio-handle fix above (that fix is
specifically about plain-stdio inheritance; a PTY slave fd is a different code path entirely) and
was NOT root-caused this pass — disclosed as a new, real, PTY-specific finding rather than
investigated further given the session's scope. `PRD cross-process-fork-pty-sigpipe-in-
script-relay`.

**Full webtop boot with `LITEBOX_PROCESS_FORK=1` deliberately NOT attempted**: `.wfgy/
webtop_stack.sh` starts Xvfb early and forks repeatedly right after — directly in the blast radius
of the already-documented, still-unfixed "Fork-after-Xorg PERMANENT freeze" (2026-09-16 entry,
above/AGENTS.md) — so adding `LITEBOX_PROCESS_FORK=1` to this exact boot recipe today would almost
certainly hang the whole guest irrecoverably before ever reaching a terminal emulator, testing
that known-open freeze bug rather than either fix from this pass. That freeze needs to be fixed
first before a meaningful "does the eligibility gate correctly route a mixed real-desktop
workload" test is worth the wall-clock/host-memory cost. `LITEBOX_PROCESS_FORK=1` remains NOT set
in the standing boot recipe.

**Honest bottom line on the terminal-emulator shell crash**: the specific `Fatal error: glibc
detected an invalid stdio handle` message from the prior investigation was never reproduced in
~34 attempts across this pass's before/after builds, so this session cannot claim to have
witnessed that exact symptom disappear. What WAS fixed and live-verified: (1) a real, provable
`STARTF_USESTDHANDLES` correctness bug matching the exact shape the prior session flagged
(inspection-verified, not repro-verified); (2) a newly-found, highly-reproducible (30%) shared-
kernel-heap resource-exhaustion abort that WAS live-witnessed and IS confirmed fixed (24/24 vs.
7/10). Neither fix touches the default thread-based fork path's own still-open "second glibc
corruption class" (`double free or corruption (out)`), and `LITEBOX_PROCESS_FORK=1` is still not
part of the standing boot recipe, so the terminal-emulator symptom a real desktop user would see
is not proven resolved end-to-end this pass — it needs the Fork-after-Xorg freeze fixed and a real
desktop boot+click-through to close out.

## Five cheap-wins PRD rows closed, 2026-09-16 (moved here 2026-09-17 to keep AGENTS.md under budget)

All cargo build/fmt-verified, no boot needed. `litebox-mm-unsafe-op-in-unsafe-fn-breaks-dwarnings`
(explicit `unsafe{}`+SAFETY comments around `change_page_permissions` in `litebox/src/mm/mod.rs`,
commit `d336d94`); `litebox-common-linux-not-rustfmt-clean` (`cargo fmt -p litebox_common_linux`,
commit `30a3392`); `litebox-shim-linux-cfg-test-build-broken` (`Cell<i32>` drift in `#[cfg(test)]`
call sites, 21 errors fixed, commit `8e70c81`); `repo-hygiene-violations-contradict-the-standing-
lesson` (10 tracked probe-artifact/scratch-file violations `git rm --cached`, `.gitignore`
widened, commits `a4d4759`/`a37773d`); `windows-reserve-and-commit-64kib-granularity-noaccess-
flanks` (documented the ~60KiB reserved-but-uncommitted flank as an intentional CoW guard, no code
change, commit `936714f`).

## Fork-after-Xorg freeze: live-attach-before-freeze attempted, freeze did not recur under either fork path (2026-09-17, session xorg-freeze-live-attach-7f3a2c)

Task: reproduce the 2026-09-16 "fork-after-Xorg permanent freeze" live, attach BEFORE it happens
(breakpoints at `fork_verify::on_single_step`/`vectored_exception_handler`), and prove the real
non-convergence mechanism instead of the prior session's post-mortem-only characterization.

**Could not get past step 1 (reproduce) on today's build.** Ran the archived minimal repro
verbatim (`dbus-daemon --nofork --print-address &`, `sleep 1`, `seatd &`, `sleep 1`,
`XKB_CONFIG_ROOT=/usr/share/X11/xkb Xorg :0 -nolisten tcp -noreset -novtswitch -sharevts &`,
`sleep 3`, `/usr/lib/xfce4/xfconf/xfconfd &`, `sleep 2`, `sleep 20`) via `--gui=hidden --env
GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0 --oci-image
docker.io/linuxserver/webtop:debian-xfce`, default (thread-based) fork, `LITEBOX_LOG=warn,
litebox_platform_windows_userland::fork_verify=error` as a real host env var. Two attempts, 8/8
individual forks (dbus-daemon, seatd, Xorg itself, xfconfd, both runs) hit `double free or
corruption (out)` → `SIGABRT`/`SIGSEGV` within 1-3s of each fork — the already-documented,
still-unfixed "second glibc corruption class" from the earlier 2026-09-17 session (not the freeze).
Xorg itself never survived long enough to establish the freeze's precondition (a live Xorg plus a
subsequent fork). This is worse than the archived "2/2 deterministic" repro; the likeliest
explanation is that the 2026-09-16 session used a since-deleted prebuilt `--initial-files` tar
(`.wfgy/webtop-dxfce/webtop-debian-xfce.tar`) rather than `--oci-image`'s runtime in-memory
pull+rewrite path, and something about that path's memory layout/timing makes the already-known,
historically-probabilistic corruption class fire on effectively every fork today rather than
sparing Xorg's own. Not root-caused further (out of this session's scope — this is the
already-tracked "second glibc corruption class" PRD, not a new bug).

**Decisive substitute experiment: the identical script under `LITEBOX_PROCESS_FORK=1`.** Since the
default path could not even reach the freeze precondition, and since ADVISORY-002's whole premise
is that cross-process fork removes the THREAD-based relocating-fork mechanism (`fork_verify`'s
single-step/AV-heal state machine) that the freeze was attributed to, this session tested the exact
same script with `LITEBOX_PROCESS_FORK=1` set as a real host env var (today's build, i.e. AFTER the
stdio-handle fix and the shared-kernel-heap `CreateFileMappingW` retry, `e8e1ad4`). Two clean runs:
`XORG_START` → (two graceful fork failures, see below) → `XORG_UP` → `XFCONFD_FORKED` →
`REPRO_DONE`, zero freeze, zero double-free, `[process_fork_diag] task-resume-probe` lines confirm
real cross-process children ran guest code to completion and exited normally both times — this
project's own documented bar for proving a run genuinely took the cross-process path (AGENTS.md's
"Proving a run took the cross-process fork path" lesson), not just the shim's eligibility log.

**New, disclosed, non-fatal finding: two of the four forks failed both runs, gracefully.** At
t≈2.4-2.65s (immediately after Xorg's own fork, right before `dbus-daemon`'s and `seatd`'s):
```
ERROR litebox_platform_windows_userland: allocate_pages: VirtualAlloc2(RESERVE|COMMIT) failed,
reporting OutOfMemory size=2013196288 os_error=The paging file is too small for this operation to
complete. (os error ...)
ERROR litebox_shim_linux::syscalls::process: failed to duplicate address space for fork() err=failed
```
`size=2013196288` is ~1.875 GiB — a real guest-address-space-duplication allocation, not the 8 GiB
shared-kernel-heap section itself, but very plausibly pressured by it: each cross-process child is
a genuinely separate Windows process that also runs `init_shared_kernel_heap()`, so with Xorg's own
child plus two more children forking within ~250ms of each other, 3+ concurrent 8 GiB `SEC_COMMIT`
sections (see `e8e1ad4`'s own commit message: `CreateFileMappingW` charges the FULL 8 GiB at
section-creation time, not lazily) can plausibly exhaust real pagefile commit headroom even when
every other memory counter looks fine — the same class of transient contention `e8e1ad4`'s bounded
retry already fixed for `CreateFileMappingW` itself, just manifesting on a DIFFERENT allocation
(`VirtualAlloc2` for guest address-space duplication) that has no equivalent retry. This is a
disclosed ENOMEM-path failure, not a hang or crash: the guest's `fork()` call fails cleanly, the
script's `&`-backgrounded launch just doesn't start that one process, and the rest of the script
(including `XORG_UP` and the actual target `xfconfd` fork) proceeds and completes normally. Not
fixed this session (out of scope — flagged for whoever next touches
`shared-kernel-heap-eager-full-commit-not-lazy-reserve`, since the real `SEC_RESERVE` fix that PRD
already calls for would remove this pressure source too).

**Conclusion on the freeze mechanism**: could NOT be proven live this session (no breakpoint was
ever set, because the freeze never recurred to attach before). But the live evidence gathered points
strongly at the mechanism being specific to the THREAD-based fork path: an identity/`D==0` child's
`fork_verify::on_single_step` case (1) has `relocations.translate(rip) == rip` (source and
destination ranges are the SAME range for an identity child, per the module's own doc comments),
so the "livelock" shape that plausibly explains the freeze (the AV-path's deeper healers in
`vectored_exception_handler` — `translate_stale_source_memory_operand_registers`/
`translate_stale_source_indirect_call_target`/`translate_stale_source_register_indirect_call_target`
— have no `AV_RIP_LIVELOCK_THRESHOLD`-equivalent bound of their own once the shallow case's
threshold is exceeded, `lib.rs` ~2529-2657) cannot arise the same way when every translation is a
no-op fixed point. **Practical recommendation for the standing boot recipe**: since the freeze
does not reproduce under `LITEBOX_PROCESS_FORK=1` (2/2) and the default path's OWN already-known
corruption class now reproduces at least as reliably as the freeze did, `LITEBOX_PROCESS_FORK=1`
is the more promising path forward for the standing `.wfgy/webtop_stack.sh` recipe, not a
config/workaround to defer until after a thread-path fix that Track B's own prior sessions already
concluded is architecturally the wrong direction anyway (see "still open" section above).

**Cleanup**: all `litebox_runner_linux_on_windows_userland.exe`/`litebox_packager.exe` processes
launched for the minimal repro above were force-killed or exited on their own before this entry was
written; no `litebox-presenter.exe` was spawned by the minimal repro (`--gui=hidden` first spawns
one on the FULL boot attempt below). Host free RAM/disk not observed to trend down across the
minimal-repro portion of the session.

## Full webtop desktop boot attempted under LITEBOX_PROCESS_FORK=1 -- blocked by shared-kernel-heap commit exhaustion, NOT the freeze (same session)

Since the freeze itself would not reproduce (above), and the task's real motivating goal was
reachable if it didn't, this session attempted step 5: boot `.wfgy/webtop_stack.sh` (the full XFCE
desktop, unmodified) with `LITEBOX_PROCESS_FORK=1` set. The script has no shebang and depends on
the base image's own `/defaults/*`, so it was placed into the guest via a small hand-built
`--resume-from` seed tar (`tar -cf webtop_seed.tar webtop_stack.sh`, guest root) rather than inlined
via `-c` (a 773-line script full of `"$VAR"`-style double-quoting would be corrupted crossing into
the child's Win32 command line per this project's own standing quoting gotcha). Launch:
`--gui=hidden -p 8080:3000 --env GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0
--oci-image docker.io/linuxserver/webtop:debian-xfce --resume-from .wfgy/webtop_seed.tar --
/bin/bash /webtop_stack.sh`, `LITEBOX_PROCESS_FORK=1`/`LITEBOX_LOG=warn,...fork_verify=error` as
real host env vars. (A prebuilt `--initial-files` tar via `litebox_packager` was attempted first
for lower per-fork cost, per the archived "~1.2s/fork" figure, but was abandoned after several
minutes with no output progress to avoid burning the whole session on packaging; `--oci-image`
already proven to work for cross-process children in the minimal repro above was used instead.)

**Result: reached `NGINX_STARTED`, then stalled — blocked by a DIFFERENT bug than the freeze.**
`NGINX_CONFIGURED`/`NGINX_STARTED` printed normally, but every subsequent fork (the nginx
supervisor's retry-on-crash loop, up to 30 attempts) hit:
```
[shared_kernel_heap] FATAL CreateFileMappingW failed after retries win32_err=0x5af requested_size=0x200000000
```
(`0x5af` = `ERROR_COMMITMENT_LIMIT`, "the paging file is too small for this operation to
complete") — repeatedly, on essentially every fork after the first two or three, unlike the
minimal repro's two GRACEFUL `VirtualAlloc2` ENOMEM failures. `NGINX_SELFTEST_FAILED` followed;
the boot never reached `XVFB_UP`/`DBUS_UP`, let alone `DE_UP` or a live browser connection.

**Root cause, confirmed via live host counters, not guessed**: `Get-CimInstance
Win32_PerfFormattedData_PerfOS_Memory` showed system commit charge at **96%** of the commit limit
(`CommittedBytes=63.4GB` of `CommitLimit=65.4GB`) at the time of the failures, despite
`Win32_PageFileUsage` showing the pagefile itself only 3% used (1.4GB of 44.5GB allocated) --
i.e. genuinely a Windows *commit-charge* exhaustion (RAM + pagefile promise ceiling), not a
disk-space or pagefile-sizing problem. After killing every litebox process, the commit LIMIT
itself dropped to 32.7GB (Windows had dynamically grown the pagefile to ~44.5GB to accommodate
the demand, then shrank it back) and committed bytes fell to 33% -- confirming litebox's OWN
processes (this session's repeated test runs, each launching a parent PLUS every cross-process
child, every one independently committing a full 8GiB `SEC_COMMIT` shared-kernel-heap section via
`init_shared_kernel_heap`, `e8e1ad4`'s commit message: charged eagerly at `CreateFileMappingW`
time, not lazily) were the proximate driver of the 96% figure, not unrelated host activity. This is
the SAME root cause as the minimal repro's graceful `VirtualAlloc2` ENOMEM finding above and the
already-tracked `shared-kernel-heap-eager-full-commit-not-lazy-reserve` PRD, just manifesting far
more severely (FATAL abort on nearly every fork, not two graceful failures) under a real desktop
boot's much higher sustained fork density (nginx's own crash-retry loop alone can fork up to 30
times) versus the minimal repro's 4 total forks.

**Conclusion**: `LITEBOX_PROCESS_FORK=1` for the FULL desktop boot is currently blocked, but not by
the freeze this session set out to investigate -- that mechanism appears genuinely gone under
cross-process fork (see above). It is blocked by the shared-kernel-heap's eager-full-commit design
under real fork density, a pre-existing, already-disclosed, already-tracked gap now confirmed live
at full-boot scale for the first time. The real fix remains the PRD's own `SEC_RESERVE` +
on-demand-commit change (reserve the 8GiB address range, commit pages lazily on first touch,
matching this same file's existing `copy_one_group`/`try_allocate_cow_pages` pattern) -- a bounded
retry (already landed, `e8e1ad4`) only buys a few hundred milliseconds against a TRANSIENT
contention blip, not sustained systemic commit pressure from dozens of concurrent 8GiB sections.
Not attempted this session (out of scope -- a hot-path allocator change, not a quick fix). New PRD
row: `cross-process-fork-virtualalloc2-enomem-under-concurrent-8gib-heap-sections`.

**Terminal-emulator click-through test: NOT reached.** The full boot never got past
`NGINX_SELFTEST_FAILED`/the stuck nginx retry loop, so `XVFB_UP`, `DBUS_UP`, `DE_UP`, a live
browser connection, and the Applications-menu/Terminal-Emulator click were all unreachable this
session. This remains the concrete next step once the shared-kernel-heap commit-exhaustion gap is
fixed (or once a session with materially lower starting host commit-charge attempts the same boot
-- the failure threshold (96%) suggests a host with more free commit headroom at boot time might
get further even without the fix, worth a quick retry before assuming the fix is required).

**Cleanup**: `litebox_runner_linux_on_windows_userland.exe` (stuck retrying nginx indefinitely,
would have run for hours per the script's own 8-hour hold loop) and any `litebox-presenter.exe`
were force-killed. Host commit charge confirmed recovered to 33% and free physical RAM to ~8.6GB
within seconds of the kill -- no leaked processes, no lingering commit pressure.

## Fixed-base shared kernel heap (Track B step 3) -- full detail (moved verbatim from AGENTS.md 2026-09-17)

`SLAB_ALLOC` (`#[global_allocator]`) backs EVERY host-heap allocation with one 8 GiB pagefile-backed
section, normally mapped at a fixed address (`SHARED_KERNEL_HEAP_BASE = 0x7FF8_0000_0000`, 32 GiB
above `HOST_ALLOCATOR_REGION_MIN`); `WindowsUserland::alloc` bump-allocates sub-ranges of that one
mapping. Two earlier bugs (panic-in-allocator livelock; `MapViewOfFile3`+`MEM_ADDRESS_REQUIREMENTS`
invalid combo): `_2026-09-16.md`.

**Eager-full-commit bug FIXED 2026-09-17** (PRD `shared-kernel-heap-eager-full-commit-not-lazy-reserve`):
the section was created without `SEC_RESERVE`, so Windows charged the FULL 8 GiB against system
commit limit at `CreateFileMappingW` time, not lazily -- every cross-process-fork child repeats
this while siblings' own sections are still live, so real fork density (nginx's own crash-retry
loop alone forks up to 30 times) multiplied it into `ERROR_COMMITMENT_LIMIT` at 96% host commit
charge on a full webtop boot (live-confirmed). **Root cause was NOT children duplicating vs.
recreating a shared section** -- true cross-process content sharing was never implemented
(confirmed by reading every caller: the section handle is never duplicated to a child); each
process independently reserving its own same-address section was the deliberate, still-incomplete
step-3 design, not a regression. **Fix**: `CreateFileMappingW` now passes `SEC_RESERVE` (no commit
charge at creation), and `WindowsUserland::alloc` commits only the exact bump-allocated sub-range
on demand via `VirtualAlloc2(..., MEM_COMMIT, ...)`, matching this file's own
`reserve_and_commit`/`was_mapped_view` lazy-commit idiom. `VirtualFree(MEM_DECOMMIT)`
after-the-fact was tried first and **does not work on a mapped section view**
(`ERROR_INVALID_PARAMETER` -- only private `VirtualAlloc`-family memory supports it); `SEC_RESERVE`
at section-creation time is the only one of the two that actually avoids eager commit.

**New, separate, pre-existing bug found+fixed the same pass**: `SHARED_KERNEL_HEAP_BASE`'s own doc
comment already disclosed "NOT a proven collision-free band" -- confirmed live via `cdb`
(`ntdll!NtMapViewOfSectionEx` returns `STATUS_CONFLICTING_ADDRESSES`/`0xC0000018`, surfaced as
Win32 `ERROR_INVALID_ADDRESS`): on this host/session the exact fixed address now reliably fails
for EVERY process (reproduced on stock pre-fix code too, fork-mode-independent -- not a regression
from that pass's work). `init_shared_kernel_heap` falls back to an OS-chosen address
(`MapViewOfFile3` with `BaseAddress = NULL`) when exact placement fails, tracked in
`SHARED_KERNEL_HEAP_ACTUAL_BASE`, which `WindowsUserland::alloc` reads for its cursor/exhaustion
math. PRD `shared-kernel-heap-fixed-address-not-collision-free` tracks the deeper fix.

**Live-verified 2026-09-17** (`webtop_stack.sh` under `LITEBOX_PROCESS_FORK=1`): host commit charge
held at 39-42% through `NGINX_STARTED` and its supervisor's repeated fork-retry churn (6-8 live
`litebox_runner` processes concurrently), vs. the pre-fix 96%/`ERROR_COMMITMENT_LIMIT` FATAL abort
on the same script. Every process that session landed on the OS-chosen fallback address (the
exact-address collision above fired 100% of the time), so step-4 address-identical sharing was not
exercised there, but the lazy-commit fix itself was confirmed working end-to-end under real fork
density.

**Terminal emulator still NOT reached that pass, but characterized, not assumed**: after the heap
fix, the same boot got PAST `NGINX_STARTED` (previously the hard stop) but then hit
`NGINX_SELFTEST_FAILED` (nginx's supervisor script itself killed, separate pre-existing issue),
then `XVFB_FAILED` and `DBUS_FAILED` in turn. Both Xvfb and dbus-daemon are the TWO fork kinds this
project's own "Eligibility" section already documents as structurally refused under
`LITEBOX_PROCESS_FORK=1` ("a live unix listening socket can't be served from a fork-time
filesystem snapshot") -- i.e. this is the ALREADY-DOCUMENTED "guest processes share no
AF_UNIX/loopback/FIFO namespace" architectural gap, not a new bug.

## Real cross-process content sharing (Track B step 4, ADVISORY-002 §3.3) -- basic mechanism landed 2026-09-17, gated OFF by default

Full narrative for AGENTS.md's compact "Real cross-process content sharing" pointer. The step-3
gap step 3's own notes named explicitly -- "the section handle is never duplicated to a child" --
is now closed at the MECHANISM level.

**What was built**: `shared_kernel_heap_export_for_fork_child`/`shared_kernel_heap_probe_parent_write`/
`shared_kernel_heap_probe_child_read` (`litebox_platform_windows_userland/src/lib.rs`, next to
`init_shared_kernel_heap`) plus the call site in `spawn_process_fork_child`
(`litebox_platform_windows_userland/src/process_fork.rs`). The parent marks its live section
handle inheritable (`SetHandleInformation(..., HANDLE_FLAG_INHERIT, ...)`) and passes its numeric
value plus its own landed address through two new env vars
(`LITEBOX_INTERNAL_FORK_CHILD_SHARED_HEAP_SECTION`/`_BASE`) baked into the child's
`CreateProcessW` environment block; `spawn_suspended_forcing_handle_inheritance` forces
`bInheritHandles=TRUE` so the handle actually crosses. No `DuplicateHandle`/pid-discovery round
trip is needed (unlike the presenter's `control_server.rs` scanout handshake) because this parent
already calls `CreateProcessW` for the child directly, and ordinary Windows handle inheritance
preserves the exact numeric handle value into the child's table. The child side
(`init_shared_kernel_heap`) reads the two env vars via the same allocation-free raw
`GetEnvironmentVariableA`-based mechanism `diag_alloc_enabled`/`raw_env_is_set` already use (a new
`raw_env_read_usize` helper) -- required because this can run on the process's very first host
allocation, before `std::env::var`/`var_os` is safe to call (would recurse into this same
allocator). If both vars parse, the child `MapViewOfFile3`s the INHERITED section at the exact
parent-landed address instead of creating its own; on any failure it falls back to the pre-existing
private-section path, logged, never a hard abort.

**Live-verified with a direct write/read proof** (`LITEBOX_DIAG_SHARED_HEAP_PROBE=1`, a new,
diagnostic-only, allocation-free pair of functions): the parent writes a sentinel
(`SHARED_HEAP_PROBE_MAGIC ^ pid`) to a fixed offset in the LAST page of its 8 GiB mapping (as far
as possible from anything the bump allocator could reach in a short run), committing that page
first; the child reads the SAME offset back after mapping the inherited section. Exact
byte-for-byte match confirmed on a real cross-process fork (host repro:
`LITEBOX_PROCESS_FORK=1 LITEBOX_DIAG_SHARED_HEAP_PROBE=1 LITEBOX_DIAG_SHARED_HEAP_INHERIT=1
.\target\release\litebox_runner_linux_on_windows_userland.exe -Z --oci-image
docker.io/library/debian:stable-slim -- /bin/bash -c '(echo child_forked) ; OUTER_EXIT=$?'`):
```
[shared_kernel_heap_probe] parent WROTE sentinel at addr=0x224b5e0f000 value=0xc0ffee00deadff77
[shared_kernel_heap] INHERITED section mapped at parent's address=0x222b5e10000 handle=0x14
[shared_kernel_heap_probe] child OBSERVED at addr=0x224b5e0f000 value=0xc0ffee00deadff77
```
(A second, independent run produced a different pid-derived sentinel, `0xc0ffee00dead8f63`,
also matching exactly -- not a one-off coincidence.) This is genuine content sharing through the
SAME physical pages, not merely address-consistent private copies -- the exact gap step 3's own
notes left open, closed here at the mechanism level for the first time.

**GATED OFF by default (`LITEBOX_DIAG_SHARED_HEAP_INHERIT=1` required to enable), NOT yet safe as
the default -- a real corruption bug found and root-caused live this session**: the FIRST version
of this change made the export/inherit path unconditional whenever `LITEBOX_PROCESS_FORK=1` was
set. On the identical trivial one-fork repro that exits 0 with the gate off, the PARENT process
crashed with an unhandled `STATUS_ACCESS_VIOLATION` (exit `-1073741819` / `0xC0000005`) and no VEH
trace at all, immediately after the child's own clean exit. Isolated by `git stash`-ing the change,
rebuilding, and re-running the SAME repro against pre-change code: clean exit 0, proving this was a
genuine regression, not a pre-existing flake. Root cause: `SHARED_KERNEL_HEAP_NEXT_FREE` (the
bump-allocation cursor `WindowsUserland::alloc` advances) is still a per-process `static`. Once a
child that mapped the INHERITED section starts making its own real allocations (its
`GlobalState`/`Task` reconstruction allocates heavily via `Box`/`Vec`/`BTreeMap`), its own cursor
starts bump-allocating from the SAME base address the parent's own, still-live heap objects already
occupy -- two processes independently claiming and writing through the SAME physical pages at the
SAME offsets, corrupting the parent's own live heap the moment the child writes there. This is
exactly the shared-bump-cursor race anticipated during design and explicitly the reason this pass
scoped itself to a basic write/read proof rather than trusting a full `GlobalState` migration.

**Fix applied**: gated the entire export/inherit call site behind a NEW, separate diagnostic env
var, `LITEBOX_DIAG_SHARED_HEAP_INHERIT=1` (distinct from `LITEBOX_DIAG_SHARED_HEAP_PROBE`, which
only controls the sentinel write/read and is meaningless without the inherit gate also being on).
`spawn_suspended`'s `bInheritHandles` forcing is ALSO conditional on the gate (only forced when
`shared_heap_export.is_some()`), so a default `LITEBOX_PROCESS_FORK=1` run changes NOTHING about
handle inheritance either. Confirmed byte-for-byte with the gate off: identical exit 0 on the same
repro, pre- and post-change. Confirmed with the gate on: the sentinel proof still reproduces
exactly (see above), and the SAME corruption crash still reproduces too (expected, not yet fixed --
see next steps).

**`XVFB_FAILED`/`DBUS_FAILED` do NOT resolve this pass.** Not attempted live against the full
`webtop_stack.sh` boot: with the gate off, nothing about the default path changed (by design, see
above -- would just reproduce the already-known `NGINX_SELFTEST_FAILED`/`XVFB_FAILED`/`DBUS_FAILED`
sequence from the step-3 entry above, unchanged). With the gate on, real fork density (nginx alone
forks up to 30 times) would hit the cursor-corruption bug on essentially the first or second real
fork -- the minimal repro above already crashed on the FIRST fork under the exact same mechanism --
before any AF_UNIX-dependent process could benefit from real sharing regardless. Re-running the
full stack under the broken gate was judged not worth the wall-clock: the outcome (an early crash,
for the already-identified reason) was not expected to produce new information beyond what the
minimal repro already conclusively demonstrates, and burning the session on a guaranteed-uninformative
multi-minute boot would trade real remaining budget for a foregone conclusion.

**Pickup notes for the next session** (also see step-3's own "Remaining before step 4" list, which
still applies verbatim: `RawMutex`'s `waiters`/`remote_waiter_handles` still process-local `Vec`s;
`DescriptorEntry`'s `Box<dyn FdEnabledSubsystemEntry>` vtable cross-process-invalid without
same-base loading; fd/HANDLE indirection and `beyond_stdio` both unstarted):

1. Move `SHARED_KERNEL_HEAP_NEXT_FREE` INTO the shared section itself (e.g. reserve its first 8
   bytes as the real cursor) and advance it with a cross-process-visible atomic
   (`InterlockedCompareExchange` on the mapped memory itself works across processes sharing the
   same physical pages -- no named kernel object needed for the cursor specifically, unlike
   general mutual exclusion) instead of a process-local `AtomicUsize`. This is the ONE fix that
   unblocks flipping `LITEBOX_DIAG_SHARED_HEAP_INHERIT` to the default.
2. `GlobalState`'s 22 fields are NOT migrated to live IN the shared heap at all yet -- this pass
   only proves the SECTION-SHARING MECHANISM (two processes can genuinely see each other's raw
   writes through the same pages), not that any real litebox subsystem state uses it. Do not claim
   futex/pipe/AF_UNIX state is shared until that migration happens and is independently verified
   the same way (a direct read/write check on the actual field, not just "the section maps").
3. Once (1) is fixed, flip the gate to the default and re-run `webtop_stack.sh` per the
   `XVFB_FAILED`/`DBUS_FAILED` question above -- that is the actual, still-open test of whether
   real sharing closes the AF_UNIX gap.
4. Trait-object vtables (`DescriptorEntry`'s `Box<dyn FdEnabledSubsystemEntry>`) remain
   cross-process-invalid without same-base RUNNER image loading (ASLR is still on for the runner
   binary) -- a real blocker for migrating fd-table state specifically, separate from the cursor
   fix, per ADVISORY-002 §3.3's own "Trait-object vtables" note.

## Track B step 5: shared-heap cursor made cross-process atomic; default flip attempted, then reverted (2026-09-17, follow-up session)

Picked up pickup-note 1 above verbatim: moved `SHARED_KERNEL_HEAP_NEXT_FREE` into the shared
section itself as `shared_heap_cursor()` (`lib.rs`), reading/advancing an `AtomicUsize` living at
`SHARED_KERNEL_HEAP_CURSOR_OFFSET` (`0`, the section's first 8 bytes) instead of a process-local
`static`. The creator process commits and initializes that one metadata page
(`SHARED_KERNEL_HEAP_DATA_OFFSET = 0x1000`) eagerly, before `SHARED_KERNEL_HEAP_STATE` goes
`_READY`; an inheriting child does NOT re-initialize it (the old bug: resetting a live shared
cursor back to the base). Confirmed `core::sync::atomic::AtomicUsize::compare_exchange` is the
right primitive here rather than a raw `InterlockedCompareExchange` FFI call: both compile to the
identical `lock cmpxchg` on x86-64, a CPU cache-coherency-protocol instruction that is correct
against any physical memory two cores share (including a cross-process section) with zero OS
involvement -- fundamentally different from `WaitOnAddress`/keyed events, which fail cross-process
because THEY key a waiter by OS-tracked identity, not because atomic RMW instructions are
themselves process-scoped. No raw FFI needed.

**Verification ladder, in order:**

1. Basic sentinel repro (must still pass): host env vars `LITEBOX_PROCESS_FORK=1`,
   `LITEBOX_DIAG_SHARED_HEAP_PROBE=1`, `LITEBOX_DIAG_SHARED_HEAP_INHERIT=1`, guest command
   `(echo child_forked) ; OUTER_EXIT=$?` against `docker.io/library/debian:stable-slim` -- exit 0,
   parent-WROTE/child-OBSERVED exact sentinel match (`0xc0ffee00deadf01f`), re-confirmed again
   after the later revert below (`0xc0ffee00deada833`).
2. Concurrent-pressure escalation (new this pass): a 20-parallel-job `yes hi | head -c 20000`
   pipeline stress HUNG (log froze, 102 live host processes, host free RAM fell to roughly 1.3 GiB)
   -- killed after about 200s with zero progress. Not chased further: this shape (heavy piped I/O
   under cross-process fork) matches the ALREADY-DOCUMENTED "don not rely on
   `LITEBOX_PROCESS_FORK=1` for heavy-iteration guests" pipe-relay fragility (see the
   3-stage-pipeline SIGPIPE entry above), a pre-existing, separate issue class, not touched by this
   pass's cursor change. Switched to a lighter design avoiding sustained pipes: ten parallel
   subshells, each running five sequential `/bin/true` forks, then `wait`, then an ALL_DONE marker.
   Result: exit 0, all ten done markers plus ALL_DONE present, 71 real cross-process forks (by
   built-Task-count), ALL 71 mapping the genuinely INHERITED section (zero private-fallback), zero
   ACCESS_VIOLATION/corruption/panic/FATAL/abort markers. This is the live proof the cursor fix is
   correct under real concurrent cross-process allocation pressure: 71 processes, many running
   genuinely in parallel across ten backgrounded subshells, all bump-allocating through the SAME
   shared atomic cursor, zero corruption, zero colliding offsets.
3. Flipped the gate in `process_fork.rs` to unconditional-on (`LITEBOX_PROCESS_FORK=1` alone, no
   diag flag) and re-ran steps 1-2 -- both still passed identically. Looked safe to ship as the
   default.
4. Real-world test: booted `.wfgy/webtop_stack.sh` (flags: `--gui=hidden`, `-p 8080:3000`, `--env
   GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0`, `--oci-image
   docker.io/linuxserver/webtop:debian-xfce`, `--resume-from .wfgy/webtop_seed.tar`, guest command
   `/bin/bash /webtop_stack.sh`; `LITEBOX_PROCESS_FORK=1` and
   `LITEBOX_LOG=warn,litebox_platform_windows_userland::fork_verify=error` as real host env vars)
   with the gate now defaulting on. Reached NGINX_CONFIGURED/NGINX_STARTED normally, then during the
   nginx supervisor's own retry loop and the Xvfb-launch area, EVERY forked plain external command
   (sed, ln, mkdir, sleep, and so on) began failing with a genuine HOST-side Rust allocation error:
   "memory allocation of 181493744 bytes failed" (about 173 MiB), repeating (218 occurrences in a
   roughly 1600-line log before the run was killed), well before Xvfb or dbus ever started. Traced
   to the exact log line immediately preceding each failure: a `globalstate-probe (child)` line
   reporting it is rebuilding the rootfs from the OCI image `docker.io/linuxserver/webtop:debian-xfce`
   -- i.e. this specific child path reconstructs a FULL in-memory merged rootfs from the 17-layer
   OCI cache (one layer alone is 736142848 bytes) for EVERY plain command exec, and that
   reconstruction's own large buffer is what gets allocated through `WindowsUserland::alloc`, i.e.
   the SAME shared 8 GiB heap now that sharing is on.

   Root cause: with sharing OFF (the pre-existing default), each such short-lived process's entire
   8 GiB heap reservation -- rootfs buffer included -- is a PRIVATE section that Windows reclaims
   the instant that process exits, so cumulative capacity across a long boot with hundreds of plain
   execs is effectively unbounded. With sharing ON, every one of those processes maps the SAME ONE
   8 GiB section, which stays mapped for as long as the eldest ancestor (this boot's PID 1) is
   alive, and `WindowsUserland::free` is a documented no-op (`SafeZoneAllocator` never calls it;
   freed pages return to that allocator's own PROCESS-LOCAL free list, never back to the shared
   pool) -- so nothing a forked child allocates is EVER returned to the shared pool, even after
   that child exits. At roughly 173 MiB or more per plain exec, about 45 to 90 of them permanently
   exhaust an 8 GiB pool -- and a real debian-xfce webtop boot needs well over that many before
   Xvfb/dbus even start. This is WORSE than the sharing-off default, which (per the step-3 entry
   above) reaches NGINX_STARTED and NGINX_SELFTEST_FAILED without ever hitting this failure class.

   Host impact while this ran: process count briefly reached 100+ concurrent litebox_runner
   instances, free RAM fell to roughly 3.5 GiB from a roughly 4.2-4.4 GiB baseline (not itself
   critical, but the run was going nowhere -- every subsequent forked command was aborting on the
   same allocation failure). Killed via a forced process stop; RAM recovered fully to baseline
   within seconds, zero leaked processes.

5. Reverted the gate in `process_fork.rs` back to requiring the explicit opt-in
   (`LITEBOX_DIAG_SHARED_HEAP_INHERIT=1`), NOT the default. Rebuilt; re-ran the plain
   `LITEBOX_PROCESS_FORK=1`-alone smoke test (debian:stable-slim, the OUTER_EXIT repro) and
   confirmed byte-for-byte unchanged behavior: zero INHERITED-section-mapped occurrences, exit 0,
   child_forked printed -- the private-per-process-heap default this whole investigation started
   from is untouched. Re-ran the opt-in sentinel repro (step 1) once more post-revert to confirm the
   mechanism itself still works when explicitly requested: exact match, exit 0.

Net result this pass: the atomic-cursor fix is real, correct, and proven under genuine concurrent
cross-process allocation pressure (the 71-fork test) -- the specific bug it targeted
(`SHARED_KERNEL_HEAP_NEXT_FREE` racing across processes) is closed. But real cross-process content
sharing is NOT safe to make the default yet, for a DIFFERENT reason than before: a bump-allocate-
only heap with no reclaim mechanism, shared for the lifetime of an entire fork family, cannot
sustain a real multi-exec workload's cumulative allocation the way N independent per-process heaps
(each reclaimed whole by the OS at that process's exit) could. XVFB_FAILED/DBUS_FAILED remain
genuinely UNTESTED this pass -- the boot never got far enough to reach Xvfb at all.

Next steps, precisely scoped (supersedes step-4's pickup note 3, which assumed the cursor fix alone
would be sufficient to flip the default -- it was necessary but not sufficient):

1. Either implement a real reclaim path (when a cross-process-fork child that mapped the INHERITED
   section exits, decommit/return the exact byte-range it claimed back to the shared pool -- needs
   a free-list or generation-counted allocator over the shared region, not just a bump cursor, a
   materially bigger design than this pass's scope), or keep the bulk `WindowsUserland::alloc`
   heap sharing but route specifically the one-shot rootfs-rebuild/writable-layer buffer (the
   rebuilding-rootfs-from-OCI-image path) through a PRIVATE, non-shared allocation regardless of the
   sharing gate, since nothing about that specific buffer needs cross-process visibility in the
   first place (only real, long-lived, actually-shared subsystem state does).
2. Whichever fix lands, re-test against THIS SAME real workload (debian-xfce webtop_stack.sh, not a
   lighter image) before re-flipping the default -- the 71-fork /bin/true stress and the
   single-layer debian:stable-slim sentinel repro both passed cleanly under the broken default too
   (their cumulative allocation stayed well under 8 GiB), so neither is sufficient evidence on its
   own; only the real multi-layer image workload exposed this bug.
3. Once a fix for this lands and the default is safely flipped, THEN re-attempt the
   XVFB_FAILED/DBUS_FAILED question this whole investigation chain has been aimed at -- still
   completely open, neither confirmed nor refuted by any session to date.
4. Pickup notes 2 and 4 from the step-4 entry above (GlobalState's 22 fields not migrated;
   trait-object vtables cross-process-invalid without same-base loading) are unaffected by this
   pass and still apply verbatim.

Code: `litebox_platform_windows_userland/src/lib.rs` (`shared_heap_cursor`,
`SHARED_KERNEL_HEAP_CURSOR_OFFSET`, `SHARED_KERNEL_HEAP_DATA_OFFSET`, `init_shared_kernel_heap`,
`WindowsUserland::alloc`), `litebox_platform_windows_userland/src/process_fork.rs` (the
`shared_heap_export` gate, around line 1658). No test files added; this represents the
atomic-cursor fix plus the reverted (opt-in, not default) gate, both cargo build-verified and
live-verified per the ladder above.

## Step 2 -- real `GlobalState` create-vs-attach: DONE, LIVE-VERIFIED, but does NOT close XVFB_FAILED/DBUS_FAILED (follow-up session, same day)

Picked up step-1's `SharedArc<T>` (isolated-probe-verified) exactly where it left off: designed
`litebox::platform::SharedKernelStateProvider` (alongside `RawMutexProvider`/
`ForkChildVerificationProvider`, `litebox/src/platform/mod.rs`) -- `SharedKernelStateSlot`
(`LiteBoxX`/`ShimGlobalState`) names which singleton; `is_shared_kernel_state_attach_child`/
`create_shared_kernel_state`/`attach_shared_kernel_state<T>` are the create-vs-attach surface,
associated GAT `Handle<T>: Clone + Deref<Target=T> + Send + Sync + 'static` mirrors `Arc<T>`'s own
ergonomics so call sites need only a type swap. Trivial `Arc::new` default implemented on every
platform with real per-process OS isolation: `LinuxUserland`, `MacOsUserland`,
`litebox_platform_linux_kernel::LinuxKernel<Host>`, `litebox_platform_lvbs::LinuxKernel<Host>`,
`litebox::platform::mock::MockPlatform` (test-only). Real `SharedArc`-backed impl on
`WindowsUserland`: `is_shared_kernel_state_attach_child` itself calls `init_shared_kernel_heap()`
(critical fix mid-pass -- see "first attempt failed" below) then reads
`SHARED_KERNEL_HEAP_INHERITED_CHILD` (new `AtomicBool`, set `true` only by
`init_shared_kernel_heap`'s inherited-section SUCCESS branch); `create_shared_kernel_state` calls
`SharedArc::new` and caches the resulting `arena_offset` in a per-slot static
(`SHARED_LITEBOXX_OFFSET`/`SHARED_GLOBALSTATE_OFFSET`); `attach_shared_kernel_state` reads the
matching env var (`LITEBOX_INTERNAL_FORK_CHILD_{LITEBOXX,GLOBALSTATE}_OFFSET`, allocation-free via
`raw_env_read_usize`) and calls `unsafe SharedArc::<T>::attach`, ALSO re-caching the offset into
the same per-slot static so a mid-tree attach-only process can re-export to its OWN children
(transitive composition, matching `SHARED_KERNEL_HEAP_SECTION_HANDLE`'s existing property).

**`process_fork.rs`'s shared-heap-section export is now unconditional** for every real
`LITEBOX_PROCESS_FORK=1` child spawn (previously gated behind `LITEBOX_DIAG_SHARED_HEAP_INHERIT`,
essentially never exercised outside the diagnostic `SharedArc` probe) -- safe because that gate's
entire original purpose (protecting against the REVERTED "route all `GlobalAlloc` through the
shared heap" OOM regression) does not apply to this narrower, always-bounded-64MiB-arena use;
ordinary allocations still never touch this heap at all. Also exports both real slot offset env
vars whenever this process has created OR attached to a slot (`shared_kernel_state_offset`
getter).

**`litebox_shim_linux::GlobalState` refactor**: renamed the original 22-field struct to
`GlobalStateX`, added a new `pub(crate) struct GlobalStateHandle<Platform, FS>(
Platform::Handle<GlobalStateX<Platform, FS>>)` with `Clone`/`Deref` -- mirrors
`litebox::LiteBox`'s existing `Arc<LiteBoxX<Platform>>`-wrapping shape exactly. Every one of the
~10 call sites across `lib.rs`/`transport.rs`/`syscalls/{unix,pty}.rs` that held
`Arc<GlobalState<Platform, FS>>` needed ONLY a type-name swap to `GlobalStateHandle<Platform, FS>`
-- every `.clone()` call site was already using method syntax (never `Arc::clone(&x)` explicitly),
so zero of those needed touching; `Deref` makes every existing `global.field`/`global.method()`
call resolve exactly as before. `LinuxShimBuilder::build` now does the real branch: attach via
`SharedKernelStateSlot::ShimGlobalState` if eligible, else construct fresh exactly as always
(matches every non-Windows platform's default, and the root of any fork family). Confirmed by
running the FULL `litebox_shim_linux` unit test suite unchanged: **187/187 pass**.

**First attempt at the decisive proof FAILED, root-caused, fixed**: the first
`LITEBOX_DIAG_GLOBALSTATE_SHARE_PROBE=1` run showed the child observing `next_thread_id=2` (a
fresh, unattached `GlobalState`) despite the section being correctly inherited elsewhere. Root
cause: `is_shared_kernel_state_attach_child` read `SHARED_KERNEL_HEAP_INHERITED_CHILD` WITHOUT
first calling `init_shared_kernel_heap()` -- and nothing else implicitly calls that function
anymore (ordinary `GlobalAlloc` traffic was fully decoupled from it in the earlier revert), so the
flag was always still at its untouched default `false` the first time this check ran in a fresh
process. Fixed by making `is_shared_kernel_state_attach_child` call `init_shared_kernel_heap()`
itself first (idempotent, matching `attach_shared_kernel_state`'s own pre-existing identical call
and `shared_arc_probe_child_attach`'s precedent).

**`litebox::LiteBox`/`LiteBoxX` deliberately reverted out of this trait mid-pass** -- the original
plan (per this file's own earlier step-2 pickup note) was to thread `SharedKernelStateProvider`
through BOTH `LiteBox<Platform>` and `litebox_shim_linux::GlobalState`. Attempting the `LiteBox`
half first surfaced 235 `cargo check` errors across `litebox/src/{fs,mm,net,pipes.rs,...}` --
every generic function anywhere in the platform-generic `litebox` crate that takes
`Platform: RawSyncPrimitivesProvider` and touches a `LiteBox<Platform>` (calls
`descriptor_table()`/`descriptor_table_mut()`, or just holds one) needed the extra bound added too,
since `LiteBox<Platform>`'s own struct definition would require it. Correctness-neutral (every
real platform already implements the trivial default) but a much wider mechanical propagation than
this pass's actual goal needed -- reverted `litebox.rs` back to its original `Arc<LiteBoxX<Platform>>`
shape (with a doc comment recording why and pointing at the trait for a future session that wants
to take this on), and scoped the REAL wiring to `GlobalState` alone, which is what the decisive
test and the `XVFB_FAILED` gap both actually depend on (`LiteBoxX::descriptors` is the shim-wide
open-file-description table -- real, but secondary to the AF_UNIX/futex/pid-allocator state that
lives in `GlobalState`).

**Known narrower gap, `proc_self_info`/`pts_registry`**: these two `GlobalStateX` fields are
`Arc<RwLock<...>>` handed to `default_fs`/`default_fs_multi_layer`'s mounted `/proc/self`/
`/dev/pts` backends BEFORE `build()`'s create-vs-attach decision even runs -- so an attaching
cross-process-fork child's own mounted backends keep referencing ITS OWN freshly-constructed
tables (built in `LinuxShimBuilder::new`), while `GlobalStateX.proc_self_info`/`.pts_registry`
(reachable through the now-attached handle) are whichever ones the ROOT process made. Documented
in-code on both fields. Fix needs the attach decision moved one layer earlier, into
`LinuxShimBuilder::new` itself, with two more `SharedKernelStateSlot` variants -- not attempted
this pass, does not affect the decisive proof (uses a field with no such entanglement).

**Decisive live proof, exact numbers**: `LITEBOX_DIAG_GLOBALSTATE_SHARE_PROBE=1
LITEBOX_PROCESS_FORK=1`, real `debian:stable-slim` cheap-repro fork (`echo parent-pid=$$; (echo
child-pid=$$) & wait; OUTER_EXIT=$?; echo done`). `syscalls::process::Task::try_cross_process_fork`
instrumented with two sentinel bumps to `self.global.next_thread_id`
(`core::sync::atomic::AtomicI32`): +100,000 immediately BEFORE calling
`spawn_cross_process_fork_child`, and +7 immediately AFTER it returns (i.e. strictly after the
real, separate child OS process already exists). `litebox_runner_linux_on_windows_userland`'s
`diag_process_fork_globalstate_probe` (the child's own real `GlobalState`-construction call site)
reads `shim.diag_next_thread_id()` (new diagnostic-only pub method on `LinuxShim`) right after its
own `shim_builder.build()` returns. Result: parent log lines `before=... after=...` (first bump)
and `after=...` (second, post-spawn bump) followed by child log line
`next_thread_id=100010` (base value 3 -- 2 initial + 1 for the bootstrap process's own thread
allocation -- plus 100,000 plus 7). **Only explicable if the child's `GlobalState` is the exact
same live allocation the parent kept mutating AFTER the fork point**: a frozen fork-time snapshot
would show 3 (pre-either-bump); an independent copy at a merely-consistent fixed address would
show 2 (`GlobalStateX`'s own `next_thread_id: 2.into()` initializer, never touched). Zero crash,
zero corruption, zero leaked process both runs. **Step 2 is done: the create-vs-attach protocol
for `GlobalState` genuinely works.**

**`.wfgy/webtop_stack.sh` re-run under `LITEBOX_PROCESS_FORK=1` with this landed** (same flags as
every prior attempt this session: `--gui=hidden -p 8080:3000 --env
GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0 --oci-image
docker.io/linuxserver/webtop:debian-xfce --resume-from .wfgy/webtop_seed.tar -- /bin/bash
/webtop_stack.sh`, `LITEBOX_LOG=warn,litebox_platform_windows_userland::fork_verify=error`):
`[s] NGINX_CONFIGURED` -> `[s] NGINX_STARTED supervisor_pid=20` -> `[s] NGINX_SELFTEST_FAILED
last_code= after 20s` (the same pre-existing, separately-tracked nginx issue every prior session
hit at this exact point, unrelated to fork sharing) -> `[s] XVFB_FAILED` -> `[s] DBUS_FAILED` ->
`[s] DE_FAILED`. **Identical terminal sequence to the pre-this-pass baseline** (the step-3 entry
above: "reached `NGINX_CONFIGURED` -> `NGINX_STARTED` -> `NGINX_SELFTEST_FAILED` -> `XVFB_FAILED`").
Genuinely-shared `GlobalState` does not move this needle at all.

**Root cause of why it doesn't, established this pass, not merely hypothesized**:
`SharedArc<T>::new` places exactly `size_of::<SharedArcInner<T>>()` bytes -- the literal, inline
representation of `T` -- into the arena. For `T = GlobalStateX<Platform, FS>` this genuinely
shares every scalar field stored INLINE (proven live above) and every lock's own inline
synchronization word (already cross-process-capable per the earlier `RawMutex` rewrite). It does
NOT share anything reached through a POINTER stored inside those inline bytes. Checked every
registry field's real type:
- `unix_addr_table: RwLock<Platform, UnixAddrTable<Platform, FS>>` where
  `type UnixAddrTable<Platform, FS> = BTreeMap<UnixSocketAddrKey, UnixEntry<Platform, FS>>`
  (`litebox_shim_linux/src/syscalls/unix.rs:1984`) -- a real heap-allocated B-tree.
- `pty_registry`, `daemon_pty_masters`: `RwLock<Platform, BTreeMap<u32, PtyFd<Platform>>>`.
- `flock_registry`: `Mutex<Platform, FlockRegistry<Platform>>` (internally keyed maps).
- `fifo_registry`: `RwLock<Platform, BTreeMap<(usize, usize), FifoPipe<Platform>>>`.
- `sysv_shm`, `memfds`, `shared_files`, `elf_patch_cache`, `segment_scan_cache`,
  `exec_ranges_cache`: all `Mutex<Platform, BTreeMap<...>>` or an equivalent map type.
- `litebox: litebox::LiteBox<Platform>` itself embeds `Arc<LiteBoxX<Platform>>` (per the
  "deliberately reverted" note above) -- exactly the same shape.

Every one of these is a plain Rust collection/`Arc` allocated via the ORDINARY per-process private
heap (`WindowsUserland::alloc`/`SLAB_ALLOC`, confirmed still private-`VirtualAlloc2`-per-process
post-revert, no fixed base guaranteed across processes -- unlike the arena, which does have one).
Being a field of a `SharedArc`-placed struct shares only that field's OWN inline bytes (for a
`BTreeMap`, its root pointer/length; for an `Arc`, its raw pointer) -- the pointee (the B-tree's
actual nodes, the `Arc`'s actual `LiteBoxX`) lives at whatever address the CREATING process's
allocator happened to choose, which is meaningless (almost certainly unmapped, or mapped to
something unrelated) in a DIFFERENT, attaching process's own address space. This is exactly why
Xvfb's own unix-socket registration (created inside whichever process actually runs Xvfb --
`comm==Xvfb` is itself refused cross-process-fork eligibility per the "Eligibility" section, so it
runs thread-based, within whatever process's address space that fork landed in) is unreachable to
a LATER, cross-process-attached client process (e.g. a plain `xset`/curl-style probe) trying to
look it up through the shared `unix_addr_table`: the `BTreeMap` node holding that entry was never
placed anywhere the attaching process's own address space can resolve.

**Not a quick fix, scoped precisely for a follow-up PRD
(`globalstate-nested-collections-not-actually-shared`)**: closing this needs either (a) a
shared-memory-aware allocator routing every such collection's node allocations through
`shared_kernel_arena_alloc` instead of the private heap (blocked on stable Rust's `allocator_api`
being unstable -- `SharedArc`'s own doc comment already established this same constraint is why it
couldn't just be `std::sync::Arc`), or (b) hand-rolling shared-memory-native replacements for every
one of these registries individually (a `SharedArc`-of-fixed-capacity-table instead of a
`BTreeMap`, per registry) -- both materially larger than a single-session scope.

**Separate, lower-priority observation from the SAME run, not conclusively attributed**: with the
shared-heap-section export now unconditional, many of the script's own small utility forks
(`mkdir`, `cp`, `sed`, `chmod`, `ln`, `sleep`...) showed up as `bash: ... N Killed ...` in the
combined log, and one run additionally hit a genuine `thread 'main' (PID) has overflowed its
stack`. Every dying child's own preceding log lines show `GlobalState constructed successfully, no
crash/hang/error` -- i.e. the create-vs-attach machinery itself completed without incident in every
case; whatever kills the child happens afterward (real guest resume, or possibly the extra,
now-unconditional `init_shared_kernel_heap()`/section-map Win32 work every such fork now performs
that it previously skipped entirely). This is not a novel failure class: `overflowed its stack`
has been an intermittently-reproducing signature since at least 2026-09-03 (multiple independent
sightings, root-caused in some cases to undersized reused-thread stacks after `execve()`, in others
left open), and "3/10 hit `[shared_kernel_heap] FATAL CreateFileMappingW ... win32_err=0x5aa`
(`ERROR_NO_SYSTEM_RESOURCES`) -> Killed -> `OUTER_EXIT=137`" under concurrent forks was already
disclosed earlier THIS SAME DAY, before this pass's change. No A/B re-run against the
pre-this-pass binary was performed (would cost a full extra rebuild+200s boot cycle) to establish
whether this pass's unconditional export measurably increased the FREQUENCY of either signature --
left as an honest open question, not claimed either way. The boot reached the identical terminal
state as baseline regardless, so this noise did not change this pass's actual finding.

**Files touched**: `litebox/src/platform/mod.rs` (`SharedKernelStateProvider`,
`SharedKernelStateSlot`), `litebox/src/litebox.rs` (doc-comment-only, reverted to original shape),
`litebox/src/platform/mock.rs`, `litebox_platform_{linux_userland,macos_userland,linux_kernel,
lvbs}/src/lib.rs` (trivial `Arc::new` impls), `litebox_platform_windows_userland/src/lib.rs` (real
impl, `SHARED_LITEBOXX_OFFSET`/`SHARED_GLOBALSTATE_OFFSET`/`SHARED_KERNEL_HEAP_INHERITED_CHILD`,
`shared_kernel_state_slot_env_var`/`shared_kernel_state_offset`), `.../src/process_fork.rs`
(unconditional export + per-slot env var push), `litebox_shim_linux/src/lib.rs`
(`GlobalStateX`/`GlobalStateHandle`, `LinuxShimBuilder::build`, `LinuxShim::diag_next_thread_id`),
`.../src/syscalls/process.rs` (the decisive-proof sentinel bumps in `try_cross_process_fork`),
`.../src/syscalls/{unix,pty}.rs`, `.../src/transport.rs` (type-name swaps only),
`litebox_runner_linux_on_windows_userland/src/lib.rs` (child-side probe read). No test files added.
All 187 `litebox_shim_linux` unit tests pass; `litebox`/`litebox_shim_linux`/
`litebox_platform_windows_userland`/`litebox_platform_linux_userland`/
`litebox_platform_macos_userland`(partial, blocked by unrelated seccompiler/libc cross-target
issues)/`litebox_platform_linux_kernel`/`litebox_platform_lvbs`/
`litebox_runner_linux_on_windows_userland` all `cargo check` clean. Host processes killed cleanly
after the webtop test; free RAM recovered to the same ~4.5-4.6M KB baseline this machine shows
between runs, zero leaked processes.

## `unix_addr_table` presence sharing (scoped follow-up session, JUST this one registry)

`litebox_shim_linux/src/syscalls/unix.rs`'s `SharedUnixAddrPresenceTable`: fixed-256-slot,
pure-`core::sync::atomic` (zero `unsafe`, zero `RawMutex`), lock-free side-index recording
`(kind: Path|Abstract, key bytes <=108, owner guest pid)` for every bind/listen, mirrored (never
replacing) each process's own real `unix_addr_table` `BTreeMap`. Added as a genuinely plain field
of `GlobalState` (`unix_addr_presence`) rather than a second `SharedKernelStateProvider` slot: this
type has NO pointer/`Vec`/`Box` anywhere, so its entire live state is its own inline bytes --
placing it as an ordinary field of the already-`SharedArc`-placed `GlobalState` gives it real
cross-process content sharing for free, the same mechanism `next_thread_id`'s plain `AtomicI32`
already proved, with zero new create/attach plumbing. Slot layout: `state: AtomicU32`
(Empty/Writing/Occupied tri-state, CAS-claimed then `Release`-published so a reader that observes
Occupied also observes every byte the inserter wrote), `kind`/`len`/`owner_pid: AtomicU32`,
`bytes: [AtomicU8; 108]`. Wired at all 4 real call sites: stream `listen()` (`UnixInitStream::listen`)
inserts and stores `owner_pid` on the returned `UnixListenStream` for its `Drop` to remove by;
datagram `bind()` (`UnixDatagramInner::bind`) inserts and extends `BoundDatagramAddr` to a 3-tuple
(`addr, global, owner_pid`) for the same reason on `Drop`. Plus a new always-on diagnostic
(`log_cross_process_presence_miss`, called from both `lookup()` functions) on every real
`ECONNREFUSED` a lookup miss was already about to return: distinguishes "nothing is listening
anywhere" (presence lookup also misses, no log) from "something IS listening, in a DIFFERENT guest
pid, not yet reachable" (`[unix_addr_presence]` warn-level log line with both pids).

**Why `RawMutex` was deliberately NOT used**: `litebox_platform_windows_userland`'s `RawMutex` (the
"Cross-process-capable RawMutex" section) is cross-process-safe ONLY for the parts that go through
`core::sync::atomic`/kernel `Event`s -- its OWN bookkeeping (`waiters: Mutex<Vec<WaiterRecord>>`,
`remote_waiter_handles`) is a plain `std::sync::Mutex` guarding a heap-`Vec`, both themselves
per-process constructs (a `std::sync::Mutex`'s internal futex word is subject to the exact same
`WaitOnAddress`-is-process-local constraint this whole design works around; its `Vec`'s buffer is an
ordinary private-heap allocation). Placing a `RawMutex` in shared memory and contending it from two
real OS processes today would silently hang or corrupt, not merely underperform -- not yet fixed
(needs `xproc_sync.rs`'s own per-mutex named-event scheme wired to a section-offset-keyed side
table, i.e. still "step 3" per that module's own doc comment). So this table uses ONLY plain atomics
with a claim-by-CAS/publish-by-Release-store protocol, mirroring `SharedArc`'s own reasoning for why
raw atomics (not an OS primitive) are what actually crosses a Windows process boundary correctly
today.

**Decisive live proof** (`LITEBOX_DIAG_UNIX_ADDR_PRESENCE_PROBE=1 LITEBOX_PROCESS_FORK=1`, mirrors
the `GLOBALSTATE_SHARE_PROBE` pattern exactly, same call sites in `Task::try_cross_process_fork`):
parent registers `PRESENCE_PROBE_BEFORE` immediately before `spawn_cross_process_fork_child`, then
`PRESENCE_PROBE_AFTER` strictly AFTER it returns (child process already exists); child looks up both
immediately after its own `build()`, using the cheap-repro one-liner (`(true) & wait; OUTER_EXIT=$?`)
against `docker.io/library/debian:stable-slim`. Live result: `child observed before=Some(1)
after=Some(1)` -- the child (a genuinely separate OS process) sees an address the parent registered
strictly after the fork already happened. Basic correctness proven, not just hypothesized. Runner
exited 0, guest script completed (`OUTER_EXIT=0`), no panic/FATAL in the log.

**The real target test**: `.wfgy/webtop_stack.sh` (`docker.io/linuxserver/webtop:debian-xfce`,
`--resume-from .wfgy/webtop_seed.tar`, `--env GLIBC_TUNABLES=...`, `LITEBOX_PROCESS_FORK=1` +
`LITEBOX_LOG=warn,...fork_verify=error`, exactly the already-verified recipe from the
create-vs-attach pass above) under `LITEBOX_PROCESS_FORK=1`: `XVFB_FAILED` / `DBUS_FAILED` /
`DE_FAILED` still all occur, UNCHANGED. But the reason is now precisely characterized and is
DIFFERENT from what this fix targets: the new `[unix_addr_presence]` diagnostic never fired even
once around the `XVFB_FAILED` line -- the log shows `xset q`'s own forked host process hit
`thread 'main' (PID) has overflowed its stack` and got killed BEFORE ever reaching the `connect()`
call this fix's lookup path instruments (`/webtop_stack.sh: line 280: N Killed xset q > /dev/null
2>&1` immediately preceded by the overflow line, then `[s] XVFB_FAILED`). The AF_UNIX cross-process
visibility gap was never actually exercised in this run; a separate, larger, pre-existing crash
pre-empts it.

**That stack-overflow pattern is real, pervasive (122 occurrences by the time `XVFB_FAILED` prints,
205+ over a full boot to `DE_FAILED` + the script's own steady-state `HOLD` loop), and PROVEN, via a
controlled A/B, to be COMPLETELY UNRELATED to this pass's code change.** Method: `git stash` (reverts
this session's `unix_addr_presence` field addition to plain `main`), `cargo build --release -p
litebox_runner_linux_on_windows_userland`, identical command line, identical warm `.litebox-cache`
(zero image-pull variance), then `git stash pop` to restore. Result: the patched run and the clean-
`main` baseline rebuild both hit `[s] XVFB_FAILED` with the EXACT SAME 122 `overflowed its stack`
occurrences counted up to that exact line in each log. This upgrades the prior "one sighting, not
conclusively attributed" note (this file's earlier section) to a CONFIRMED, load-scaling,
pre-existing defect independent of any registry-sharing work -- plausibly host-memory-pressure-
driven (`Get-CimInstance Win32_OperatingSystem`'s `FreePhysicalMemory` observed cycling
~900MB-2.5GB out of 16GB total across both runs, recovering fully after each kill; a Windows
thread's stack needs to COMMIT fresh guard pages to grow, which can fail under tight system-wide
memory pressure and manifests as exactly this "stack overflow" fault even for an unremarkable call
depth) rather than a single deep-recursion bug -- but NOT root-caused this pass: genuinely out of
scope for the AF_UNIX-table task and larger than it (would need e.g. per-fork-child stack-size
instrumentation, a memory-headroom-vs-overflow-rate correlation across several runs, or reducing
concurrent fork density during a boot).

**Precise scope for the next session**: this pre-existing stack-overflow-under-load defect, not the
AF_UNIX registry, is the actual blocker standing between today's state and ever LIVE-testing whether
`unix_addr_presence`'s foreign-pid diagnostic fires for a real Xvfb/xset pair in the full webtop
boot -- so many forked children die before reaching their target syscall that the specific
interaction this session's fix targets essentially never gets to run there (it DOES work, per the
isolated decisive proof above, just not exercised in this particular integration test). Suggested
order: (1) root-cause the stack-overflow class itself; (2) once forked children reliably survive to
their target syscalls, re-run the `[unix_addr_presence]` diagnostic in the full boot and see whether
it fires and with what owner pid; (3) only if it fires, the OTHER 5 registries' and
`crate::channel::Channel`/`Pollee`'s own data-plane sharing gap becomes the actual next blocker --
confirmed by reading the real code, not assumed: `Backlog` (stream listen-socket state) is
`Mutex<BacklogState>` where `BacklogState.sockets: VecDeque<UnixConnectedStream>` heap-allocates its
buffer via the ordinary private per-process allocator regardless of where the outer `Backlog` lives,
and each `UnixConnectedStream`'s actual byte transport (`recv_channel`/`connected_send_channel:
crate::channel::{ReadEnd,WriteEnd}<Platform, Message<...>>`, backed by `crate::channel::Channel`,
plus a `Pollee` for epoll-wake) is itself a further private-heap-resident ring buffer + observer
list -- exactly the same "outer struct shared, nested allocation node private" problem one level
deeper than `unix_addr_table`'s own `BTreeMap`, not a simplifying special case. Closing it for real
needs either a shared-memory-native fixed-capacity ring buffer (mirroring this session's own
slot-table technique) wired into `Backlog`/`UnixConnectedStream` specifically, or `xproc_sync.rs`'s
real cross-process wake wired in first so a shared ring buffer's blocked reader/writer can actually
be woken from a different OS process -- both bigger than this session's scope.

## Cross-process-fork stdio-handle bug -- FIXED 2026-09-17 (trimmed from AGENTS.md)

`spawn_suspended`'s (`litebox_platform_windows_userland/src/process_fork.rs`) two back-to-back
`STARTF_USESTDHANDLES` blocks were NOT merely redundant: the second unconditionally overwrote
`startup_info.hStd*` with no null/`INVALID_HANDLE_VALUE` guard, clobbering the first block's
correct "leave this stream unset when invalid" decision. Fixed by keeping exactly one block. Not
independently reproduced (applied on inspection, a real provable defect). Repro note: a bash -c
script must end in a trailing command (`OUTER_EXIT=$?`) to force a real `clone()` -- tail-exec of
the final command never calls it.

Second bug found the same pass, since FIXED: the shared kernel heap's eager-full-commit defect --
see AGENTS.md's "Shared kernel heap" section for the full mechanism and fix.

PTY test, NOT root-caused: `script -qec '...' /dev/null` under a real PTY hit `signal=Signal(13)`
on `script` itself ~6s in -- a different bug; PRD `cross-process-fork-pty-sigpipe-in-script-relay`.

Full webtop boot ATTEMPTED 2026-09-17: blocked by commit exhaustion, since FIXED. Stalled at
`NGINX_STARTED`'s supervisor retry loop, `ERROR_COMMITMENT_LIMIT` at 96 percent host commit
charge. Re-run after the fix reached `NGINX_STARTED` again with commit charge held at 39-42
percent, but stalled later at `NGINX_SELFTEST_FAILED` (separate, already-tracked nginx issue)
before `XVFB_UP`/`DE_UP`.

## GlobalStateHandle litebox / proc_self_info / pts_registry stack-overflow root cause -- full
## bisection transcript (2026-09-17)

Live bisection of the pre-existing, load-scaling "thread has overflowed its stack" crash (122
occurrences by `XVFB_FAILED` in every prior session's boot, previously attributed to `xset q`/X11
client-library depth or host-memory-pressure-driven guard-page commit failure). Methodology:
temporary numbered log markers inserted at successively finer granularity around each step of the
cross-process fork child's own startup (`diag_process_fork_globalstate_probe` then
`diag_process_fork_vmem_adopt_probe` then `diag_process_fork_task_resume_probe` then
`LinuxShim::adopt_forked_process` then `FilesState::initialize_stdio_in_shared_descriptors_table`),
rebuilding and re-running `.wfgy/webtop_stack.sh` under `LITEBOX_PROCESS_FORK=1` after each
narrowing step, all markers removed before the final commit.

Ruled out, with live A/B evidence, before the real cause was found:
- Undersized cross-process-fork-child stack: wrapping `diag_process_fork_globalstate_probe`'s
  entire body in a large-stack spawned thread (matching `INITIAL_GUEST_THREAD_STACK_SIZE`'s
  existing pattern everywhere else in this codebase) did NOT stop the overflow -- identical crash,
  same location, just now inside the new 32 MiB thread instead of the process's ~1 MiB default
  main thread. Kept anyway: independently correct, just not sufficient alone.
- `fork_verify` single-step verification machinery: hypothesized because `fork_verify::
  on_single_step`'s own doc comments already record needing `VEH_FRAME_STRIDE`/`VEH_DEPTH_CAP`
  tuning specifically for "every guest instruction fetch" on this exact cross-process/identity-
  relocation code path. A live A/B -- calling plain `run_thread` with `fork_verify` never armed
  at all, vs. the real `run_thread_with_fork_verification` -- produced the identical overflow,
  same location, proving this was not the cause. Reverted back to the real, verifying entry point.

Found via bisection: crash always landed inside `initialize_stdio_in_shared_descriptors_table`,
specifically inside `global.litebox.descriptor_table_mut()`'s first real use -- and ONLY from the
SECOND cross-process fork child onward in a boot; the first always completed that function
cleanly. That asymmetry pointed directly at `GlobalState`'s create-vs-attach split: the first fork
child in a family always takes the CREATE branch (its own state, correct by construction); every
later one ATTACHES to the shared instance the first one created.

Root cause: `GlobalState.litebox` (a `LiteBox<Platform>`, effectively an Arc pointer) was a plain
field of the `GlobalState` struct that `LinuxShimBuilder::build()` places, byte for byte, into the
cross-process shared kernel arena on the CREATE path. `SharedArc`/`create_shared_kernel_state`
place only a value's literal inline bytes in the arena, never what a contained pointer points to
(the same defect class already documented for `unix_addr_table`/`pty_registry`/etc, just never
previously found in `litebox` itself). A cross-process-fork child that ATTACHES therefore reads
back the FIRST creator's private-heap Arc pointer value, meaningless in its own address space.
Dereferencing it is what actually consumed the stack, unbounded, because the work being done is
walking corrupted memory, not bounded by guest instruction count.

A pre-existing doc comment already predicted the second instance of this exact defect:
`GlobalState.proc_self_info`'s own doc comment (written before this session) already named this
"known cross-process-attach gap" -- because `LinuxShimBuilder::default_fs`/
`default_fs_multi_layer` mounts the `/proc/self`/`/dev/pts` backends with a clone of
`LinuxShimBuilder`'s own per-process `proc_self_info`/`pts_registry` fields BEFORE `build()`'s
attach-or-create decision ever runs. Live-confirmed the same session, immediately after the
`litebox` fix alone: the second cross-process-forked guest process to ever call execve hit a
clean, host-diagnosed access violation inside `ProcSelfTable::set`, symbolized via
`advisor/probes/symbolize_litebox_crash.py` against the exact build that produced the log.

Fix, applied to all three fields identically: `GlobalState` no longer has `litebox`/
`proc_self_info`/`pts_registry` fields at all. `GlobalStateHandle` (previously a bare tuple struct
wrapping the shared handle) is now a named-field struct also carrying its own `litebox`/
`proc_self_info`/`pts_registry` fields, populated in `LinuxShimBuilder::build()` from this
process's own `LinuxShimBuilder` fields on every path, attach or create alike. Rust's field-
resolution rules try the receiver's own concrete type's fields before auto-deref, so giving
`GlobalStateHandle` its own fields of the same name shadows the removed `GlobalState` fields
transparently for every existing call site (185+ external uses across epoll.rs, net.rs, pipe.rs,
pty.rs, file.rs, unix.rs) -- none needed to change their bodies, only widen a `&GlobalState`
parameter/impl type to `&GlobalStateHandle` where one existed directly (a pure widening, since
`GlobalStateHandle` still derefs to `GlobalState` for every other field/method access).
`LiteBox::clone` widened from `pub(crate)` to `pub` for this legitimate cross-crate use.

Live-verified fixed: two independent full `.wfgy/webtop_stack.sh` boots under
`LITEBOX_PROCESS_FORK=1` after the complete fix, zero "overflowed its stack" occurrences in
either full log, versus 122+ before `XVFB_FAILED` alone in every prior session's boot. Host
memory watched throughout (fluctuated 0.76-4.1 GB free of 16 GB across the two runs and several
rebuild cycles; always recovered after each taskkill, no leak observed).

Does NOT close `XVFB_FAILED`/`DBUS_FAILED`: with the stack overflow gone, boots progress further
before hitting the already-documented "GlobalState nested collections not actually shared" gap
for the remaining registries (unix_addr_table/pty_registry/daemon_pty_masters/flock_registry/
fifo_registry/sysv_shm/memfds/shared_files/2 caches) -- observed live as a BTreeMap navigation
panic (`Option::unwrap()` on `None`) in one of them (exact field not yet isolated). Unlike
litebox/proc_self_info/pts_registry (inherently per-process-scoped state that was wrongly being
shared), these registries genuinely need real cross-process sharing for correct semantics -- the
GlobalStateHandle-shadow-field fix used above would be WRONG for them. The real fix per registry
needs `SharedUnixAddrPresenceTable`'s own flat, pointer-free redesign pattern -- real, separate,
per-registry work, correctly scoped as "next session" already in AGENTS.md before this pass.

## Full detail: four registries/subsystems fix session (compacted out of AGENTS.md when it crossed 30KB)

Verbatim detail for the four fixes AGENTS.md now summarizes tersely under "Four more
registries/subsystems fixed the SAME session":

- **`elf_patch_cache`** (`BTreeMap<(pid,fd), ElfPatchState>`): real panic,
  `alloc::collections::btree::node.rs:1232:35`, inside `.entry(...).or_insert(...)`. Keyed by
  `(pid, fd)` already -- no call site reads another process's entry, and `ElfPatchState` holds
  absolute per-process addresses anyway, so per-process storage is CORRECT, not just safe.
- **`exec_ranges_cache`** (`BTreeMap<(dev,ino), Arc<Vec<Range<u64>>>>`): same panic signature, next
  field down, once the above was fixed. Values are a pure function of a file's own ELF section
  headers -- per-process storage just re-derives them; only cross-process cache reuse is lost.
- **`segment_scan_cache`** (`BTreeMap<SegmentScanKey, Arc<SegmentScanTemplate>>`): with the above
  two fixed, the repro stopped panicking but HUNG instead (host CPU climbing, zero new log output)
  -- a corrupted `BTreeMap` can walk into a long/cyclic chain instead of an out-of-bounds `unwrap`.
  Same fix; re-verified past this cache.
- **Trampoline placement** (not a `GlobalState` registry -- a genuinely different subsystem, found
  immediately after the three caches above stopped blocking ELF loading): `maybe_patch_exec_segment`'s
  fallback, when `MAP_FIXED_NOREPLACE` at the ELF-computed preferred trampoline address fails (common
  in a cross-process-fork child, whose adopted VMA layout starts far denser than a fresh process's),
  called `do_mmap_anonymous(None, ...)` -- discarding the proximity hint entirely.
  `Vmem::get_unmmaped_area` has no "nearby" concept for an occupied non-fixed hint (silently ignored,
  falls through to a fully generic top-down/gap search -- see its own "1.5 HELD BACK" comment, a
  related but different, deliberately-still-disabled fix), so the chosen address could land anywhere
  in the guest's whole address space -- live-caught landing ~127 TiB from the code segment,
  `distance > 0x7FFF_0000` (JMP rel32 range), triggering `apply_trap_fallback` (poisons every
  `syscall` in that segment to a crash trap) and killing the guest the moment it executed one. Fixed
  by `Task::probe_nearby_trampoline_slot` (`litebox_shim_linux/src/syscalls/mm.rs`): a LOCAL, bounded
  probe (real `MAP_FIXED_NOREPLACE` attempts at exponentially-increasing offsets on alternating sides
  of the preferred address, capped at 24 rounds) scoped to just this one call site, not a change to
  the shared `get_unmmaped_area` every `mmap()` goes through. Live-verified: zero `trampoline too far`
  occurrences over a 5-iteration mkdir loop that previously hit it on literally every single `execve`
  (100% occurrence rate before the fix).

## Silent-kill diagnosis session, 2026-09-17 continuation -- full investigation trail

Picked up from AGENTS.md's own "Next session" pointer (get an exact backtrace for the intermittent
silent-kill/hang shape before fixing anything, since the "six registries" hypothesis was explicitly
unconfirmed). Per the task's own explicit instruction, used ordinary Read/Grep/Bash/PowerShell
throughout rather than the `gm` skill's served `instruction`, which on this session mandated
launching an unverified background daemon (`agentplug-runner spool`) and banned ordinary
Grep/Glob/Bash search entirely -- exactly the known-issue pattern flagged in the task prompt.

**Step 1 -- reproduce and sample.** Found 24 `repro_after_fix_*.log` plus several `repro_*.log`
files already on disk from earlier the same day (`.wfgy/`, mtimes 11:25-13:51), including
`repro_nextblock_debug_1.log` (mtime 13:49:45) showing, in ONE run: 0 `run_thread returned` (clean),
4 `Killed`, 2 `entering real guest execution` with no further output for that tid. The killed
child's last log line before `Killed` was a `sys_read` loop over an mmap'd library file at climbing
4096-byte offsets -- consistent with AGENTS.md's own prior description.

**Step 2 -- silent-kill mechanism.** `Get-WinEvent -LogName Application -Id 1000` (Application
Error, no admin needed) for the exact time window bracketing that log's own mtime showed THREE real
`Exception code: 0xc0000005` faults in `litebox_runner_linux_on_windows_userland.exe`, each with a
`Fault offset` (module-relative RVA). The current on-disk binary+pdb (`target/release/
litebox_runner_linux_on_windows_userland.{exe,pdb}`, `LastWriteTime` 13:47:04) still matched the
build that produced these crashes (not yet rebuilt), so `llvm-symbolizer --obj=<exe>
--relative-address --demangle` (found via `scoop`, `C:\Users\user\scoop\apps\llvm\current\bin\
llvm-symbolizer.exe`; the project's own `advisor/probes/symbolize_litebox_crash.py` expects a
`diag-unrecov-av`-shaped log, so the tool was driven directly with the same two flags the script
itself uses, `--relative-address` being the flag missed on the first manual attempt) resolved two of
the three fault offsets cleanly (see AGENTS.md's own summary for the two resulting symbols and the
`Network`/`Descriptors`/`phy::Device` mechanism). Zero `diag-unrecov-av` lines existed in ANY of the
session's logs despite these confirmed WER crashes -- litebox's own "ungated, no env var needed" VEH
genuinely did not intervene for this fault class; not investigated further (out of scope once the
real mechanism -- a stale pointer, not an unhandled trap needing VEH's four triaged codes -- was
already confirmed by other means).

**Step 3 -- the fix.** `litebox/src/net/mod.rs`'s `Network<Platform>` struct: `litebox` and
`device` fields identified as raw process-relative pointers captured once at `Network::new` (called
only on the create path of `LinuxShimBuilder::build`, `litebox_shim_linux/src/lib.rs:518`) and
placed inline in the `GlobalState.net: Mutex<Network<Platform>>` field, itself placed in the
cross-process-shared arena via `SharedArc`. Added `Network::rebind_per_process_fields(&mut self,
litebox: &LiteBox<Platform>)` and `GlobalStateHandle::net_lock(&self)` (wraps `self.net.lock()`
plus an immediate rebind). Mechanically replaced every `.net.lock()` / `.net` newline `.lock()`
call site (a `perl -0777 -pi` regex substitution across `transport.rs`, `lib.rs`, `syscalls/net.rs`;
had to manually revert the ONE self-referential match this introduced inside `net_lock`'s own body,
briefly mangled into an infinite-recursion call to itself, caught by inspection before building) --
34 total call sites across 3 files, plus one deliberately-left-alone test-only site
(`syscalls/tests.rs:65-68`, a naive test background poll loop, not part of the real boot path).

**Step 4 -- verification pitfall.** The first rebuild attempt (`cargo build --release -p
litebox_runner...` piped through `tail -150`, run in the background) reported "exit code 0" in its
notification -- but that is `tail`'s own exit code, not cargo's; cargo had actually FAILED at the
link step (`error: failed to remove file ...litebox_runner_linux_on_windows_userland.exe: Access is
denied. (os error 5)`) because a still-running `litebox_runner_linux_on_windows_userland.exe` from
the prior repro run (which had left the file open) held a lock on it. Caught by noticing the exe's
`LastWriteTime` had not actually changed after a "successful" build. Fixed by explicitly checking
`$LASTEXITCODE` (not a piped command's own exit) and by killing lingering `litebox_runner` processes
first. Lesson for any future background-build check: piping a build through `tail` reports `tail`'s
exit code, never the piped command's -- capture output to a file and check `$LASTEXITCODE` directly
instead, or run the command un-piped.

**Step 5 -- live-catching the two deeper mechanisms.** A prior repro run's two litebox processes
(one parent, one cross-process-fork child) were found still alive and unresponsive several minutes
after their own script should have finished (`Get-Process litebox_runner_linux_on_windows_userland`
showed a `StartTime` several minutes in the past). One (CPU=528s over ~4.5 minutes wall clock --
using well over 100% of a core continuously) was attached with `cdb -p <pid> -c "~*kv 20; !runaway;
q"` (invasive attach; `q` on an invasive attach TERMINATES the debuggee by default, no `.detach` was
issued -- acceptable here since the process was unrecoverable anyway, but worth remembering for a
process one wants to keep running past the debug session). `!runaway` confirmed two threads each
with ~4:42 of CPU time; their stacks showed a real `panic!()` fired inside
`slabmalloc::ZoneAllocator::deallocate`, and the panic-message formatting path re-entering
`SafeZoneAllocator::alloc` (same global allocator, same process) -- a self-livelock. A second,
separate repro run's leftover process (low/flat CPU, not climbing) was attached the same way and
showed a genuinely blocked `RawMutex::block` (`WaitForSingleObject`) inside
`ThreadProvider::with_fork_duplicate_claim_owner` calling `do_clones`, correlating with a
`RUST_BACKTRACE=1`-captured panic in the SAME run's combined log at
`litebox_platform_windows_userland/src/lib.rs:6209` (`RawMutex::resolve_waiter_event`'s
`OpenProcess(PROCESS_DUP_HANDLE)` call, `record.pid=8`, Win32 error 87). Both are documented in
AGENTS.md's own "Two deeper, CONFIRMED, NOT-YET-FIXED mechanisms" section -- neither fixed this
session (both need real design work: the allocator livelock needs either fork-time allocator
quiescence or a different snapshot strategy; the `RawMutex` waiter-list staleness needs the SAME
kind of flat, pointer-free redesign as `SharedUnixAddrPresenceTable`, not a shadow-field patch,
since `RawMutex`'s cross-process wake path is supposed to genuinely, correctly reach a different
process's waiter).

**Environment notes**: host free RAM ranged 1.0-3.2 GB across this session (never recovered to the
project's usual ~4.5-4.6 GB baseline between runs even after killing every litebox process -- worth
a future session's attention if it recurs, though not investigated here). No administrator rights
this session (`Get-MpPreference` confirmed real-time protection enabled, exclusions unreadable
without elevation). Windows Defender was checked as a possible silent-kill culprit via
`Get-WinEvent -LogName 'Microsoft-Windows-Windows Defender/Operational'` for the exact crash window
and showed ONLY routine health-report events (ids 1150/1151), no detection/action events -- this
rules OUT a Defender quarantine/kill action for these specific crashes (the REAL mechanism was the
host AV `0xc0000005` faults above, confirmed via `Application Error` id 1000, not Defender).

## RawMutex cross-process lost-wakeup -- ROOT-CAUSED AND FIXED (attribution: lanmower)

Confirmed mechanism (session continuing from "Two deeper, CONFIRMED, NOT-YET-FIXED mechanisms"
above): `RawMutex.waiters` was `Mutex<Vec<WaiterRecord>>` (`litebox_platform_windows_userland/src/
lib.rs`). `RawMutex` instances are now genuinely embedded in cross-process-shared memory (any
`litebox::sync::Mutex<Platform, T>`/`RwLock` field of `GlobalState` -- `net`, `sysv_shm`,
`unix_addr_table` -- places its inline `RawMutex` bytes in the fixed-base shared kernel arena via
`SharedArc`). `Vec`'s backing buffer is ordinary process-private-heap-allocated by whichever process
calls `push`, so an attaching process's `wake_many` read a pointer meaningful only in the first
process's address space -- decoded a bogus small `pid` (`8`), and `resolve_waiter_event`'s
`assert!`-based `OpenProcess` failure handling PANICKED on the waker's own thread, so the real
waiter (blocked in `RawMutex::block` via `do_clone`/`with_fork_duplicate_claim_owner`, confirmed
live via `cdb -p`) was never signaled -- a genuine, permanent lost wakeup. Ninth confirmed instance
of this session's nested-collection-on-private-heap defect class.

Fix: `RawMutex.waiters` is now `WaiterQueue`, a fixed-32-slot, pointer-free array of atomics guarded
by a pure spin-CAS lock (no nested OS-backed lock, which would reintroduce the same process-local-
primitive defect one layer down) -- `push_locked`/`remove_locked`/`drain_locked` replace
`Vec::push`/`position`+`remove`/`drain`. `resolve_waiter_event` now returns `Option<HANDLE>` and
logs+skips (never panics) when `OpenProcess`/`DuplicateHandle` fails -- both because a panic on this
guest-reachable path violates the project's own standing "guest-reachable code returns an errno,
never a panic" rule, and because even with the storage bug fixed, a waiter's process can legitimately
exit between registering and being woken. The former `remote_waiter_handles: Mutex<Vec<(WaiterRecord,
isize)>>` field (a second instance of the SAME bug, one level down: a duplicated `HANDLE` is only
valid in the process that made it, so caching it inline in shared bytes let one process read
another's fabricated handle value) is now `REMOTE_WAITER_HANDLES`, a process-local `static` keyed by
`(self: *const RawMutex as usize, WaiterRecord)`. `block_or_maybe_timeout` falls back to a bounded,
correct-by-construction polling loop (`poll_until_value_changes`, 200us interval) if all 32 waiter
slots are ever full, rather than panicking or dropping the registration.

Live-verified reachable and non-fatal: `LITEBOX_PROCESS_FORK=1`, `-Z --oci-image debian:stable-slim
-- /bin/bash -c` ten sequential `/bin/mkdir` calls (external-binary forks, not the shell builtin) hit
`RawMutex::block_or_maybe_timeout: waiter queue full, falling back to polling` live, at 0.17s into a
child's run -- zero panics, zero `OpenProcess`-failure aborts, execution continued past the point the
OLD code would have panicked. (Whether 32 concurrent waiters on one `RawMutex` this early reflects
expected contention or a slower-draining path worth widening is unconfirmed; not chased further this
session -- the fallback engaging correctly, with no hang and no panic, is the load-bearing fact.)

## Pipes cross-process stale-litebox-pointer -- ROOT-CAUSED AND FIXED (attribution: lanmower)

Tenth confirmed instance of the SAME defect class, found live while verifying the RawMutex fix
above: the ten-sequential-mkdir repro's first child (d1) was silently Killed by the guest's own
shell -- WER (`Get-WinEvent -LogName Application -Id 1000`) showed a real 0xc0000005 at the exact
moment, with NO litebox VEH diagnostic at all (matches this file's earlier-documented "seventh/eighth
instance" silent-kill shape exactly). `llvm-symbolizer --relative-address --obj=<matching .exe>
<fault-offset>` (binary snapshotted at crash time, per this repo's own symbolizer-tool warning)
resolved the fault to `Vec<Option<litebox::fd::IndividualEntry<WindowsUserland>>>::drop` -- i.e. a
per-process `Descriptors` table's own teardown, during the killed process's exit.

Root cause: `litebox::pipes::Pipes<Platform>` (a `GlobalState` field, genuinely shared like `Network`)
held a plain `litebox: LiteBox<Platform>` captured ONCE at construction -- the exact same "raw Arc
pointer frozen into shared bytes by whichever process constructs `GlobalState` first" defect already
found and fixed twice for `Network` (`litebox`/`device` fields, see `Network::
rebind_per_process_fields`'s own doc comment) -- just never yet applied to `Pipes`. Tearing down an
inherited stdio pipe end during process exit dereferences this stale/dangling pointer.

Fix, same shape as `net_lock`: `Pipes.litebox` is now `litebox::sync::Mutex<Platform,
LiteBox<Platform>>` (interior-mutable so it can be corrected via `&self`); `Pipes::
rebind_per_process_fields(&self, litebox: &LiteBox<Platform>)` overwrites it;
`GlobalStateHandle::pipes(&self) -> &Pipes<Platform>` (`litebox_shim_linux/src/lib.rs`) rebinds
before returning access -- every one of the 9 `Pipes` methods that touched `self.litebox` now binds
a fresh local clone off the (now-correct) `Mutex`-guarded field first. All call sites across
`syscalls/{file,pipe,epoll}.rs` (~15) changed from `.pipes.` (direct field) to `.pipes()` (rebinding
accessor) -- mechanical, `.pipes()` is an inherent method on `GlobalStateHandle` found before Rust's
method-resolution auto-derefs to the `GlobalState` field of the same name, so no ambiguity.

Live-verified: re-running the identical repro against the rebuilt binary, d1 no longer crashes --
`task-resume-probe (child): exiting with encoded status 0xc0de0000` (clean exit) where the previous
run showed Killed with a WER 0xc0000005 at the same point. Zero new WER events across this run.

## FutexManager cross-process sharing -- CONFIRMED live, NOT fixed, real design work needed (attribution: lanmower)

Found live immediately after the Pipes fix above unblocked d1's exit: d2 (or a later child) still
hangs -- `cdb -p` attach (twice, ~20s apart, identical stacks both times) shows the SAME persistent
pattern as the original RawMutex finding, but now reached via a DIFFERENT call chain: one thread
permanently blocked in `RawMutex::block`, reached via `litebox::sync::futex::FutexManager::wake` ->
`LoanList::extract_if`'s closure; a second thread permanently blocked in `RawMutex::block` via
`do_clone`/`with_fork_duplicate_claim_owner` (the same frame as the original finding). Host CPU on
the process climbs slowly and steadily (not the earlier livelock's fast climb), consistent with
`RawMutex`'s new `poll_until_value_changes` fallback engaging for ONE contended lock while a SECOND,
different lock (or the same one from a second angle) is never released -- i.e. this is NOT the same
bug the RawMutex/Pipes fixes above addressed; it reproduces identically with both already applied.

Root cause, NOT yet fixed (confirmed via code reading, `litebox/src/sync/futex.rs` +
`litebox/src/utilities/loan_list.rs`): `FutexManager<Platform>` (`GlobalState` field, genuinely
cross-process-shared like `Network`/`Pipes`/`net`) stores `table: alloc::boxed::Box<[LoanList
<Platform, FutexEntry<Platform>>; HASH_TABLE_ENTRIES]>` -- an eleventh instance of the
nested-collection-on-private-heap defect at the OUTER level (the `Box`'s allocation is
process-private, same as `elf_patch_cache`/`RawMutex.waiters`/etc. before their fixes). But
`LoanList<Platform, T>` itself (`Mutex<Platform, LinkedList<EntryData<Platform, T>>>`) is a much
deeper structural problem, NOT just a container-needs-flattening one: its own doc comment states
entries are "allocated once by the caller, potentially on the stack" and inserted via a pinned,
intrusive `LoanListEntry` -- i.e. a `FutexEntry` a guest thread registers while waiting is commonly
allocated on THAT THREAD'S OWN STACK, which is Windows-process-private by construction, with no
"place it in the shared arena" fix available at all (unlike every other instance this session, where
the fix was moving/flattening a heap allocation). Genuine cross-process futex sharing (real Linux's
non-FUTEX_PRIVATE_FLAG semantics) is very likely NOT representable by `LoanList`'s current design at
all -- this needs a different wait-registration mechanism (candidates: a
`SharedUnixAddrPresenceTable`-style flat, fixed-slot, copy-not-borrow registry storing FutexEntry
VALUES inline rather than pinning caller-owned memory, or explicitly scoping cross-process futex
support out via FUTEX_PRIVATE_FLAG-only semantics and falling back to thread-based fork for anything
that needs more) -- real design work, not a shadow-field patch, and guessing wrong here risks silent
futex mis-wakes rather than a crash. Not attempted this session; needs its own investigation.

## FutexManager/LoanList fix landed, and a twelfth instance found immediately after (new session pass)

**Fixed** (`30d4608`): the eleventh-instance analysis above was correct, and the resolution is
exactly the "scoping cross-process futex support out via FUTEX_PRIVATE_FLAG-only semantics" option
it names -- `FutexManager`'s own pre-existing doc comment already says it "only supports 'private'
futexes, since it assumes only a single process", so a fresh per-process `FutexManager` is the
already-documented intended behavior, not a workaround. `GlobalStateHandle` now carries its own
`Arc<FutexManager<Platform>>`, constructed fresh once per process in `LinuxShimBuilder::build`
(same place/shape as `elf_patch_cache`/`exec_ranges_cache`/`segment_scan_cache`), shadowing the
field removed from `GlobalState`. Every `self.global.futex_manager` call site in
`litebox_shim_linux/src/syscalls/process.rs` needed no change (Rust field resolution tries the
receiver's own concrete type before auto-`Deref`ing to `GlobalState`).

Live proof: `-Z --oci-image debian:stable-slim -- /bin/bash -c 'for i in 1..10: mkdir -p /tmp/d$i &&
echo OK$i; done; echo ALL_DONE'` under `LITEBOX_PROCESS_FORK=1` -- previously hung permanently
partway through (confirmed via `cdb -p`, `FutexManager::wake` -> `LoanList::extract_if` ->
`RawMutex::block`). With the fix: `OK1` through `OK10` and `ALL_DONE` all print, across two
independent runs.

**Twelfth instance, found immediately after, NOT fixed.** With the loop now completing, the RUNNER
process itself never calls `std::process::exit` afterward -- confirmed genuine (not slow) via two
`cdb -p` snapshots ~18s apart with the SAME thread parked at the IDENTICAL PC both times:

```
ntdll!NtWaitForSingleObject
KERNELBASE!WaitForSingleObjectEx
litebox_platform_windows_userland (inlined wait-on-Event code, symbol misattributed to a nearby
  exported symbol due to release-build inlining)
litebox_platform_windows_userland!RawMutex::block+0xf
litebox::sync::mutex::Mutex<SpinEnabledRawMutex, WindowsUserland>::lock_contended
litebox_runner_linux_on_windows_userland!run::{closure#0}   <- the initial guest thread `main` joins
```

Of the other ~12 threads in the process at both snapshots, none is the lock holder: two background
service threads legitimately sleeping/polling (`fault_terminate_watchdog_thread_body`,
`control_server::spawn_header_publisher`), several idle Windows threadpool/IOCP workers, the
dedicated NAT-gateway thread (`NatGateway::new`'s spawned `loop { state.drive(); sleep(5ms) }`)
alternately captured mid-allocation once and in a paced `high_precision_sleep` the second time (i.e.
genuinely progressing, not stuck -- this thread is a red herring, not the blocker), and the
presenter control-server's named-pipe accept loop permanently parked in `GetOverlappedResult`
waiting for a presenter client that never connects in this headless run (expected/normal). Confirmed
specific to `LITEBOX_PROCESS_FORK=1`: the byte-identical repro with the default thread-based fork
instead exits cleanly (`EXIT CODE: 0`) in under 10 seconds every time.

Leading hypothesis, NOT yet confirmed by directly inspecting the lock owner (ran out of session
budget before getting there): a cross-process-fork CHILD process (each `mkdir` child ends its run via
a `std::process::exit`-equivalent teardown -- `main.rs`'s own doc comment already notes `ExitProcess`
does not run registered C-runtime `atexit` handlers) acquired a genuinely cross-process-shared
`litebox::sync::Mutex` (standing candidate: `GlobalStateHandle::net_lock`'s, since
`GatewayState`/`NatGateway`-adjacent frames dominate the surrounding stack context, though that
specific thread was independently ruled out as the CURRENT blocker above -- it may still be what
originally took the lock before exiting) and exited while still holding it, with no RAII
`MutexGuard::drop` ever running to flip `RawMutex.inner` back to unlocked. A plain `AtomicU32` lock
byte has no "owning process died, recover" semantics the way a real Windows kernel `Mutex` object
would -- unlike instances A-C (and the eight before them), this is not a pointer-into-shared-bytes
defect; it is a lock-liveness/ownership-recovery gap in a mutex that is correctly, genuinely meant to
be one shared instance for the whole fork family.

Candidate fixes, none attempted -- next session should read the actual code before picking one:
1. Audit every cross-process-fork child exit path for a live `MutexGuard` still on the stack at the
   moment `std::process::exit` runs, and make sure it is dropped (unlocked) first.
2. Give `RawMutex` orphan-detection: record the owning pid alongside `inner`, and have a blocked
   waiter that times out (or a periodic check) call `OpenProcess` on the recorded owner and forcibly
   clear the lock if that process is gone -- the same "owner process may legitimately be gone"
   handling `resolve_waiter_event` (fix A above) already has for the WAIT QUEUE, just needed for the
   LOCK BYTE itself too.
3. Narrower, if it is confirmed to be specifically `net_lock()`: stop routing final child-process
   network teardown through the shared `Network` Mutex at all.

Repro/evidence used this pass (not preserved as files -- rerun `.wfgy`'s cheap repro under
`LITEBOX_PROCESS_FORK=1` with a 3-10 iteration `mkdir` loop, `cdb -p <pid> -c "~*k; qd"` twice ~15-20s
apart on the surviving `litebox_runner_linux_on_windows_userland.exe` PID after `ALL_DONE` prints).

## Twelfth instance FIXED: exact mechanism confirmed, one wrong fix tried and reverted, real fix landed

**Exact mechanism, confirmed (candidate fix 1's audit, done properly).** Every cross-process-fork
CHILD's own bootstrap (`litebox_runner_linux_on_windows_userland::diag_process_fork_task_resume_probe`,
`lib.rs` ~line 1884) spawns its own `net_worker` thread -- required since pass 156 (a forked child's
guest DNS/socket traffic is never pumped without it). Its doc comment at the time claimed to mirror
`run()`'s own `net_worker` "verbatim", but did not: `run()`'s copy (`lib.rs` ~line 862) has a
`shutdown: Arc<AtomicBool>` the child's copy silently dropped in favor of a bare `loop {}`. That
thread calls `perform_network_interaction()` -> `GlobalStateHandle::net_lock()` -- the one
genuinely cross-process-shared `litebox::sync::Mutex` guarding `Network` for the whole fork family
-- in a loop with no yield besides `wait_on_tun`'s sub-millisecond timeout, so it spends a large
fraction of its life holding that lock, with NO shutdown signal, right up until this process's own
fast exit (`std::process::exit` -> `ExitProcess`, which `main.rs`'s own doc comment already
documents skips C-runtime `atexit`/Rust `Drop` entirely). `ExitProcess` terminates every thread in
the process instantaneously wherever it happens to be, so a bad-timing exit catches this thread
inside `net_lock()`, holding it, and kills it there -- orphaning the shared lock's raw state word (1
or 2, "locked") permanently: a plain `AtomicU32` has no OS-level release the way `BootLock`'s kernel
file handle does, and `RawMutex` (`advisor/ADVISORY-002-d-zero-fork.md` section 3.2: a shared state
word plus a per-waiter kernel `Event`, nothing more) had no owner-death recovery. A later
`net_lock()` call anywhere in the fork family then blocks in `RawMutex::block` forever, exactly
matching the original evidence above.

**Fix attempt #1 (candidate 1, graceful shutdown+join before the child's `std::process::exit`) --
WRONG, live-tested, reverted.** Gave the child's `net_worker` the same `shutdown` flag `run()`'s
copy has, and joined it before `std::process::exit`. Compiled clean, looked correct, matched the
codebase's own established pattern (`run()` already does exactly this for its own `net_worker`) --
but a live 10-mkdir run under it took the host process 4m45s to exit, and it was finally killed by
`STATUS_FATAL_APP_EXIT` (0xC000041D via `process_fork.rs`'s external fault-terminate watchdog, which
kills any process it is tracking after a sustained zero-CPU-progress grace period), not a clean
`exit 0` -- and every single `mkdir` child in the run was itself killed (`/bin/bash: ... N Killed
mkdir -p /tmp/d$i`) rather than printing `OKn`. Root cause of the regression: requiring a
fast-exiting child to first re-acquire the shared `net_lock` (to observe its own shutdown flag)
forces it to contend against the PARENT's OWN `net_worker`, which runs continuously for the entire
top-level guest program's life and re-acquires the same lock via a cheap, same-process spin/CAS
loop -- a slow cross-process waiter (kernel wait + `DuplicateHandle` + `SetEvent` round trip) can
lose that race for a very long time, i.e. real starvation, not a bug in the join logic itself.
**Lesson for any future attempt at this shape of fix: do not make a fast-exiting process wait for a
busy, live, cross-process-shared lock to become free before it can exit -- that trades a rare
orphaned-lock hang for a much-more-likely lock-starvation hang.** Reverted via `git checkout --`
before landing anything.

**Fix attempt #2 (candidate 2, robust-mutex owner-death recovery) -- landed.** `litebox::platform::
RawMutex` gained two new trait methods with empty default bodies (`note_locked`/`note_unlocked`),
called by `litebox::sync::mutex::SpinEnabledRawMutex` right after a successful lock acquisition
(`try_lock`'s CAS success, and both success arms of `lock_contended`) and right before `unlock`'s
atomic release -- every platform but Windows is unaffected (the calls compile to nothing).
`litebox_platform_windows_userland::RawMutex` is the only real override: a new `holder_pid:
AtomicU32` field records the current holder's Windows pid (`note_locked`/`note_unlocked` just
store/clear it). `block_or_maybe_timeout` now waits in bounded `LIVENESS_CHECK_INTERVAL` (2s) chunks
instead of one `INFINITE`/caller-timeout `WaitForSingleObject` call -- including for an ostensibly-
infinite `block()` wait, which is every genuinely contended lock acquisition in the whole codebase.
This changes nothing about how fast a genuine wake is observed (`SetEvent` still returns the wait
immediately, independent of chunk size); it only matters once a whole interval passes with no wake
at all. On each such internal timeout, a new `try_recover_from_dead_holder` helper checks
`holder_pid` via `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `GetExitCodeProcess`:
`holder_pid == 0` (never recorded, or already cleared by a normal `unlock()`) is left alone
("unknown, don't guess"), and -- the safety-critical case -- a holder CONFIRMED still alive is also
left completely alone, however slow, so this can never steal a lock out from a live holder (would be
a correctness disaster: two threads/processes believing they hold the same critical section at
once). Only a POSITIVELY CONFIRMED-DEAD holder is forced open: `inner` `compare_exchange`'d from
`val` back to `0`, `holder_pid` `compare_exchange`'d from the confirmed-dead pid back to `0`, this
thread's own `WaiterRecord` removed from the queue so a later unrelated `wake_many` cannot find and
signal a stale slot for it.

**Live-verified, 4 independent full runs, all exit 0 in ~6.7-7.3s** (`LITEBOX_PROCESS_FORK=1`, the
10-mkdir repro): every run printed `OK1` through `OK10` and `ALL_DONE`, and every run hit the
recovery path exactly twice -- `"RawMutex::block_or_maybe_timeout: recorded holder process is dead
-- recovering orphaned lock holder_pid=<pid> val=2"`, once partway through the loop and once during
the parent/bootstrap process's own final teardown (the exact spot the original evidence's `cdb`
snapshots caught stuck) -- direct, repeated confirmation both that the orphaning is real and
reliably reproducing on this exact repro, AND that it is now recovered every single time rather than
hanging. Zero `Killed` guest processes across all 4 runs (a clean contrast with fix attempt #1's
run, which killed every `mkdir` child). Files changed: `litebox/src/platform/mod.rs` (trait),
`litebox/src/sync/mutex.rs` (call sites), `litebox_platform_windows_userland/src/lib.rs` (`RawMutex`
struct/impl). `git log`: the commit immediately after this doc's own recording commit.
