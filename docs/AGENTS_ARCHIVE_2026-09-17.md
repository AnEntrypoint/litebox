# litebox archive — 2026-09-17

Trimmed out of `AGENTS.md` to keep it under the 30KB working-set budget. Read this for the trail;
`AGENTS.md` keeps only the current-state pointer.

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
