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
