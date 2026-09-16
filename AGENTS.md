# litebox — current state (2026-09-16)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, and a claim a later commit superseded, were deleted rather than hedged. Reference
detail is drained to the `docs/AGENTS_ARCHIVE_*.md` files and per-investigation logs to the dated
`docs/*.md` in the map below — read those for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one line
plus its pointer, not in a separate memory file or a pass narrative appended below.

## The cheap repro — start here

```
target/release/litebox_runner_linux_on_windows_userland.exe -Z \
  --oci-image docker.io/library/debian:stable-slim -- /bin/bash -c '<script>'
```

One ~81MB layer, `[cache] HIT` after the first pull, real GNU coreutils instead of busybox — which
matters: coreutils `touch` issues the `utimensat(fd, NULL, …)`/futimens form busybox's never reaches
(`caaac79`). Two host-side gotchas, each already costly:

- **PowerShell, never Git Bash** — Git Bash rewrites `/absolute/guest/paths` into
  `C:/Program Files/Git/...` before the runner sees them, giving a misleading `ENOENT`. **`Start-Process
  -RedirectStandardOutput/-RedirectStandardError` makes the runner exit almost instantly with zero guest
  output** (no crash dump, no event-log entry); use `& .\runner.exe ... *> combined.log` instead.
- **Single quotes only inside `-c`** — embedded double quotes are corrupted crossing into the child's
  Win32 command line. This masqueraded as deep fork/stack-pointer corruption for a whole sub-session.

**Log level**: the default is `warn,litebox_platform_windows_userland::fork_verify=error` (`EnvFilter`'s
own ERROR-only default discarded all real `warn!` sites; `fork_verify` is pinned to `error` because it
warns per single-stepped instruction). Do **not** add `LITEBOX_LOG=error` by reflex; use
`fork_verify=warn` when a fork heal is the subject.

## Standing lessons and hard constraints

- **No WSL or hypervisor, ever** — always run under the matching runner
  (`litebox_runner_linux_on_windows_userland.exe`/`litebox_runner_linux_userland`); cross-compiling FOR
  Linux is fine, running the result in a VM defeats the premise.
- **`fork_verify.rs`'s stale-pointer-healing bug class is Windows-only** (real `fork()` gives the child
  identical addresses) — never port such a fix to another platform's crate.
- **Never `bcdedit /debug on`** without a kernel debugger already attached — two full-host freezes
  needing a power-cycle.
- **Never run two full-stack verifications concurrently**, peer sessions included — starves both, and
  the failure looks exactly like a real hang. Kill every `litebox_runner` between runs; watch
  `FreePhysicalMemory` and kill on a falling trend, not a fixed RSS number.
- **`LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check**, never
  `PrintWindow`/`CopyFromScreen`. A pixel count alone never identifies WHO painted a frame — decode
  frame structure (`advisor/probes/decode_frame.py`) and correlate against `DIAG_TIMELINE execve`'s
  real argv0.
- **Never time litebox with one host process per datapoint** (a bare spawn costs 1.6-2.3s, dwarfing real
  per-exec differences) — run N iterations inside ONE guest process, establish a noise floor. Never
  subtract timestamps across a parent log and a fork-child log — `init_logging()` resets elapsed time
  to ~0 per child.
- **Refusal errno choice is API contract** — EPERM lets callers degrade, EINVAL/ENOSYS fails them hard;
  wrong choices have silently broken whole subsystems before (archive).
- **Proving a run took the cross-process fork path needs `[process_fork_diag] task-resume-probe` lines,
  never the shim's eligibility log** — that log fires regardless of whether the fork actually happened
  that way (produced two recorded false conclusions, archive).
- **fork carries pipes, regular files and the writable layer into a child, but NOT sockets** — a
  pre-fork-created listening socket serves nothing to a forked child; run dbus-daemon non-forking, and
  for XFCE use `xfce4-session`, never `startxfce4`.
- **On host-side crashes, use `advisor/probes/symbolize_litebox_crash.py`, snapshotting `.exe`+`.pdb`
  next to the log** — a ring dump's `rva=` is only meaningful against the exact emitting build.
- **Isolate the harness before blaming litebox** — launch guest probes directly as the runner's
  top-level program, never via a runtime-built `/bin/sh -c` wrapper.
- **Never trust a container tag name for its WM/session contents** — verify by registry manifest + blob
  tar-listing, or a live in-guest `/usr/bin` listing.
- **Never record a test count you did not just watch run to completion**, and never leave a suite red
  for an environmental reason. No counts are recorded here on purpose.
- **Repo hygiene** — packed layer tars, frame dumps and debug logs never go in git (`.wfgy/`, gitignored);
  untrack anything `git add -A` sweeps in by mistake.
- Procedural know-how is in the archive's "Working practices": freestanding guest binaries built on the
  HOST, probe injection via a small `--resume-from` overlay tar, mature libraries over hand-rolled code.
- **Guest-reachable code returns an errno, never a panic** — the host process IS the entire guest
  session, so an `unimplemented!()`/`unreachable!()`/panic, or unbounded recursion, on any
  guest-reachable path kills every guest process at once. Bitten many times (OOM, metadata ops, open
  flags, nested `epoll_ctl`, corrupted guest contexts); full fixed-bug list with shas: archive.

## Cross-process fork (`LITEBOX_PROCESS_FORK=1`)

A genuine `D == 0` fork — child at the SAME addresses, no relocation, no `fork_verify` healing — exists as
`spawn_cross_process_fork_child` (design case `advisor/ADVISORY-002-d-zero-fork.md`). It short-circuits to
a native fork when `platform.has_native_fork()` — the whole fd-carrying apparatus is Windows-only
scaffolding for a missing syscall.

**It is correctness-sound**: zero corruption across every completed fork on a `bash -c` loop repro, vs the
thread-based default's 100% tcache-corruption rate on the same repro (ADVISORY-001 §3N is the
**thread-based** path's defect only).

**Eligibility** — refused only for `comm` == `Xvfb`/`dbus-daemon` (a live unix listening socket can't be
served from a fork-time filesystem snapshot), an already-borrowed fd table, a beyond-stdio fd that isn't
a pipe end/path-recorded regular file/eventfd/close-on-exec (overridable by
`LITEBOX_PROCESS_FORK_IGNORE_FDS`), or an unsanitizable `fs_base`/context. On a real `debian-xfce` boot
the only remaining blocking kind is `unix-socket` — 5 refused forks of 34, down from 34/34. Per-kind
deviations: archive — read before assuming real `fork()` semantics.

**Per-fork cost** was ~3.5-5s, now ~1.2s; a full `webtop_stack.sh` boot reaches `NGINX_STARTED` in under a
minute versus never in 15+. Older rootfs-re-merge/writable-layer-growth cost explanations are
**measured wrong**. Use `LITEBOX_DIAG_FORK_TIMING=1` for the next cost question. Three correctness bugs
the perf work exposed are all fixed; mechanisms/repros/cost history: archive.

**Reading a cross-process log** — the `fork_verify` "stale CODE pointer" noise-vs-signal read is archived
(`docs/AGENTS_ARCHIVE_2026-09-15.md`).

**Still open**: nginx's own SSL-cert generation fails on its first real startup attempt — the original
symptom this investigation began from, genuinely not root-caused (`docs/track-b-fork-fix-progress.md:
146-152`). Do not cite the separate curl-self-test stall as live open work: that one is fixed.

## Container images and OCI loading

**`litebox_packager --oci-image <ref> --output <tar>`** pulls, whiteout-merges, rewrites every ELF and
produces a bootable flat tar in one command (x86-64/Apple Silicon hosts only) — supersedes the ad-hoc
OCI-pull Python scripts this project once hand-rolled, retired, do not recreate.

**Runtime in-memory loading** — `--oci-image <ref>` pulls, merges and rewrites every layer in memory; no
host directory is ever created for the rootfs (a real one hit three independent Windows-path bugs).
Rewritten layers are cached under `.litebox-cache/`, keyed so a rewriter change self-invalidates. Large
images (multi-GB, 100K+ entries) pack fine now; residual risk is host-memory contention, not a litebox
bug. `tar_ro.rs`'s multi-layer index is built ONCE at mount, not per read (was O(entries²), 17.3s →
0.35s fixed). Cache internals and the four fixed OOM bugs: archive.

**A trampoline-extension failure used to poison a whole segment's syscalls, now fixed** — sized from a
byte-pair count instead of a one-page guess, capped 4MiB (full detail archived). **Tags, verified live,
never from the name** (full detail archived): `linuxserver/webtop:alpine-mate` ships MATE not XFCE;
`alpine-xfce` doesn't exist; `debian-xfce`/`ubuntu-xfce` ship real XFCE; `edgelevel/alpine-xfce-vnc` is
Alpine 3.16.0.

**X server choice**: for the DRM/wgpu on-screen (`--gui`) path use `Xorg` with `modesetting` — litebox's
virtual DRM device is legacy-KMS + dumb-buffer + XRGB8888 only, no atomic modeset/GBM/EGL, so a GBM-first
compositor lands on its least-tested fallback, and `Xvfb` never touches DRM/KMS at all (zero page-flips,
indistinguishable from "never drew"). For browser/selkies, `Xvfb` IS correct and verified — its
`-shmem` framebuffer works now that SysV shared memory exists.

**Durable artifacts**: `C:\dev\litebox-webtop\webtop_seatd.tar` (stock MATE webtop); the
`.wfgy/xfce-build/` hand-assembled weston+XFCE tar is superseded by the stock-image path.

## A real desktop renders in a browser

**XFCE renders in a real host browser, and MATE too** — full pipeline (Xvfb, selkies/pixelflux x264,
MIT-SHM) inside litebox, only the reverse proxy host-side. Working config: selkies `--addr=0.0.0.0`
port **8081**, dashboard over `--publish`, `/websockets` tunnelled to 8081. Fourteen litebox defects
got here, all landed (archive).

**A stock s6-overlay image boots with no flags/stubs**: `/init` runs 16 cross-process children with
zero uncarriable fds into supervision — retires three "fundamental blocker" claims older notes
carried. **The black XFCE desktop was deterministic, now fixed**: the runtime rewriter corrupted
`libLLVM.so.19.1`'s `.dynsym`, so mesa `dlopen` failed forever — not a race. Settled in the archive:
PI futexes work; labwc's SIGABRT is upstream wlroots; every pre-`694bb93` gdk-pixbuf finding is stale;
two network fixes this path needed.

**XFCE also renders on the THREAD-based fork path, gated by one flag** (`docker.io/linuxserver/
webtop:debian-xfce`, `.wfgy/webtop_stack.sh`). Without it, 3/3 boots die ~7s in — ADVISORY-001 §3N's
safe-linked-tcache write (the `fork_verify: stale CODE pointer` warning right before it is a red
herring; translation is correct). Fix: `--env GLIBC_TUNABLES=glibc.malloc.tcache_count=
0:glibc.malloc.mxfast=0` as an `--env` runner flag (a bare host-shell prefix does NOT reach the guest).
Glibc-only workaround, not a fix (PRD `glibc-tunables-workaround-pending-zero-fork`);
`LITEBOX_PROCESS_FORK=1` removes the class properly but can't run a desktop until the AF_UNIX gap
below closes. Selkies also needs `--clipboard-enabled=false` (its clipboard monitor re-triggers the
same corruption every tick).

**Open here.** One client per selkies instance, no slot reclaim on reload. An intermittent host AV ends
some runs (host-allocator region fault) — separate non-determinism from the ACK-stall-kill below.
Architectural gap: **guest processes share no AF_UNIX/loopback/FIFO namespace**, so a cross-process fork
gives zero AVs but Xvfb is unreachable from its own clients — one shared host-side transport would put
the whole desktop on the crash-free path (`docs/fork-fs-veh-2026-09-08.md:128-144`).

**The glibc/tcache crash class still sporadically hits selkies**, separately from the ACK-stall-kill:
live-captured once despite the `--env` tunables flag being passed correctly (a DPI-fork on a client's
5th rapid reconnect SIGSEGVs) — genuinely ADVISORY-001 §3N on selkies' own fork; not yet re-verified
crash-free over many cycles (the ACK-stall-kill dominates the symptom in practice). **2026-09-16:
`GLIBC_TUNABLES` propagation through `spawn_exec_collision_child` has NO gap** (live-proven, true on
every collision including selkies' own python3 re-exec) — **the recurring crash is a SECOND, different
corruption signature under heavy fork load** (`double free or corruption (out)` SIGABRT, not §3N's
`REVEAL_PTR` XOR SIGSEGV), hitting bin paths the tunables deliberately leave enabled. **Track B
territory, not a tunable-coverage gap** — do not re-attempt a `GLIBC_TUNABLES`/env fix without evidence
of a THIRD mechanism. Full evidence: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

### The ACK-stall-kill and port-8081 watchdog — backpressure CLOSED, watchdog fix applied but unverified (2026-09-16)

Eight ACK-stall-kill candidates investigated earlier, seven refuted, one livelock gap fixed
(`b6ddf43`); ninth candidate (write-side backpressure) below. Two `RESOLVED` facts from earlier this
day, both still load-bearing: the python3/ET_EXEC address-collision fix
(`litebox_platform_windows_userland/src/lib.rs`, `0x600000`→`0x1000000`, 9/9 clean vs 10/10 collisions
pre-fix) and the video-never-arrives readiness-gate race fix (real browser client, full `sk.log`
pipeline, Applications-menu → Terminal Emulator opens in 1-2s). Detail: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

**Backpressure fix (`478e640`) — RESOLVED, live-verified.** The real blocker was never the send-gating
logic itself: the guest-side patcher (`advisor/patches/selkies_primary_backpressure_patch.py`, inlined
in `.wfgy/webtop_stack.sh`) crashed on EVERY prior boot with an uncaught `OSError: [Errno 38] Function
not implemented` from `shutil.copy2()`'s `copystat()` → `os.listxattr()` (litebox's Linux shim has no
`listxattr`) — `selkies.py` was **never actually patched** in any session that believed it was; every
earlier "trigger/lift work correctly, ping timeout still happens" read was against unpatched code (the
pre-existing, unrelated `_run_frame_backpressure_logic` stall detector, not this patch's send gate).
Fixed via `shutil.copyfile()` (data-only, no xattr needed for a source backup). Also found and fixed a
real logic gap in the gate itself: the per-frame backlog check compared the CURRENT backlog to the
drop threshold, not what it would become after the new frame — one oversized keyframe could sail
through while backlog was still under the cap and push it far past it in one write. Fixed: check
`backlog_bytes + len(data_chunk) > threshold`; default cap also lowered 256KiB→128KiB. **Live-verified
with the real fix applied**: a genuinely-foregrounded `chrome-devtools` tab
(`visibilityState`/`hasFocus()` confirmed throughout) through a clean, download-only asymmetric
throttle (userspace TCP proxy, 20KB/s down / unthrottled up — the clean isolation the investigation
always needed and never had) survived 60+s with zero `keepalive ping timeout`, vs. dying at ~19s
pre-fix under the identical throttle. This closes the whole multi-session ACK-stall-kill/backpressure
thread (Wake Lock refutation, `Slow 3G` bidirectional-throttle confound, etc. — history in the archive).

**Port-8081 double-bind bug — watchdog fix code-verified correct and its instrumentation confirmed
live; the double-bind condition itself did NOT recur under this session's testing, so a fire+recover
cycle is still unwitnessed.** Root cause and fix unchanged from prior passes: `kill -0 "$pid"` was the
sole gate on the stall counter and silently reset it every tick under litebox's non-standard process
model; fix drops that pre-check and adds a per-tick `SELKIES_BIND_WATCHDOG_TICK` trace line. **This
session (13 boot cycles, real `chrome-devtools`/`claude-in-chrome` browser client against
`http://127.0.0.1:3000/`, `--resume-from .wfgy/webtop_stack_seed_fixed.tar` confirmed byte-identical to
the live `.wfgy/webtop_stack.sh` before every boot):**
- 4/13 boots died to the already-known, unrelated `/bin/sh` `Signal(11)` non-determinism class before
  selkies bound; two of those specifically killed the watchdog's OWN shell (`comm=sh`, the pid printed
  by `SELKIES_BIND_WATCHDOG_STARTED`) seconds after it started — an aggravating environmental factor
  this session hit repeatedly, not a defect in the fix.
- On every boot where the watchdog shell survived, `SELKIES_BIND_WATCHDOG_TICK` fired correctly on
  cadence with accurate `count`/`last_count`/`stall` fields (confirmed across 4 separate boots) —
  **the instrumentation gap this fix targeted is closed and live-confirmed.**
- Threw substantial, varied reconnect pressure at the real bound selkies process without reproducing
  `OSError starting Data WS`: staggered real-browser reload bursts (up to 40 genuine
  `Legacy client ... connected` events and 16 server-side `reconnecting too quickly` rejections in one
  boot) and, separately, genuinely concurrent same-tick `new WebSocket(...)` floods (up to 40 at once,
  via in-page JS against `ws://127.0.0.1:3000/websockets`) producing real `close code 4029` rejections.
  `count` stayed `0` throughout every one of these — the specific overlap window (two "start Data WS"
  attempts landing on the exact same reconfiguration moment) is narrower than any of this session's
  triggers reached, consistent with the very sparse historical hit rate (one clean live capture across
  many prior sessions). **Not a refutation of the fix** — merely means the fire+kill+respawn+recover
  path is still unexercised live; next session should keep the same technique (concurrent in-page
  `WebSocket` floods are more effective than sequential reloads: they generated 3-4x the
  `reconnecting too quickly` rejections per attempt) and extend the window if host RAM allows.

RAM this session ran tighter than prior documented baselines (~4-8GB free pre-boot vs. the ~7-8GB
norm, due to concurrent unrelated host load — multiple Firefox/Chrome processes, not litebox's own
footprint) — most boots crossed the 1.5-2GB safety floor within 30-90s of selkies binding under
reconnect-storm testing and were killed immediately on catching it. **Every one of the 13 cycles fully
recovered host RAM within seconds of `Stop-Process`** (settling 5.4-8.2GB free each time) — zero host
leaks, zero unrecovered processes. Normal single-client use without reconnect-storm testing still
plateaus in the previously-documented 2.0-2.9GB range.

## Host-side crash machinery

**A fatal host fault dumps before it dies, ungated**: stack walk, `RECENT_FAULTS` ring, `RECOVERY_LOG`
print with no env var needed; a real OS minidump comes only from the repeated-identical-fault circuit
breaker. Two dump fields mislead on an old reading (`error_code` is synthesized; `is_in_guest` is
tri-state) — exact semantics: archive.

**An unexplained `0xC0000005` may be a panic** — litebox no longer treats Rust panics as panics; the
handler now enters only for the four codes it triages, registers FIRST in the chain, sizes per-depth
frames from disassembly not guesswork, and no longer lets the watchdog kill a recovered run. Narrative:
`docs/veh-exception-handler-design.md`.

**Cross-process sync on Windows is a hard platform constraint**: every native address/TID-based wait is
process-local (`WaitOnAddress`, keyed events, `NtAlertThreadByThreadId`=ACCESS_DENIED); only a shared
kernel object crosses processes. `litebox_platform_windows_userland/src/xproc_sync.rs` is a
live-verified NAMED-event mutex primitive, still unwired (its own doc comment: it wants Track B step
3's fixed-base shared section first, to key its side-table by section offset rather than address).
`RawMutex` (the trait every shim subsystem's synchronization bottoms out in) is rewired as of this pass
-- see "Cross-process-capable `RawMutex`" below, a different mechanism from `xproc_sync.rs`.

## Cross-process-capable `RawMutex` (Track B step 2, ADVISORY-002 §3.2) -- done, live-verified

`litebox_platform_windows_userland/src/lib.rs`'s `RawMutex` no longer calls
`WaitOnAddress`/`WakeByAddressSingle` (process-local per MSDN, see "hard platform constraint"
above) -- replaced with a manual wait queue plus one auto-reset kernel `Event` per OS thread. Same
trait/`underlying_atomic()`/`INIT` contract, no caller changed; `wake_many` now returns the real
popped-waiter count (was always `0` -- a pure improvement, not a behaviour requirement change).
Full internals (queue/lock-ordering, timeout-race resolution): `docs/AGENTS_ARCHIVE_2026-09-16.md`.

**Cross-process half is real code, not a stub, but genuinely untaken today**: every
`WaiterRecord` carries the waiter's pid; same-pid (always true today, since `RawMutex` still lives
in per-process heap) uses the handle directly, a different pid would use `DuplicateHandle`
(already proven live cross-process, non-admin) cached in `remote_waiter_handles`. Deliberately a
DIFFERENT design from `xproc_sync.rs`'s single named-per-mutex event (that one needs a section
offset to key its side-table by, i.e. step 3) -- `RawMutex` needed a design that works BEFORE
step 3 exists, per ADVISORY-002 §3.2.

**Live-verified** (release build, default thread-based fork, no test files): `yes hello | head -c
5000000 | wc -c` -- exact `5000000`, proving correct blocking-pipe reads/writes both directions
through the new queue with no lost data. `seq 1 3000000 | sort --parallel=4 -n | tail -3` (with an
unrelated ADVISORY-001 §3N tcache workaround env so it doesn't confound this read) -- exact correct
output, proving `sort`'s real multi-threaded pthread mutex/condvar contention (glibc futex calls,
which this trait backs) completes correctly: no hang, no deadlock, no missed wakeup, no corrupted
merge. Host RAM identical before/after, no leaked processes.

**Separate, unrelated finding, not investigated**: a 3-stage pipeline under `LITEBOX_PROCESS_FORK=1`
(its real inherited-pipe-handle fd path, not the emulated-pipe path `RawMutex` backs by default) spun
two children at high CPU with no progress for 5+ minutes, killed not root-caused -- follow-up needed
before relying on `LITEBOX_PROCESS_FORK=1` for anything pipe-heavy.

**Track B step 3 (fixed-base shared kernel heap) -- NOT started, needed next.** `RawMutex`'s
`waiters`/`remote_waiter_handles` stay ordinary process-local `std::sync::Mutex`es until this lands
(then need to become POD/cross-process-safe, e.g. a fixed slot array, not a `Vec`) -- deliberately
not built yet, pending step 3's allocator seam. Full pointer-rich state inventory (the two heap
singletons that must move, the trait-object-vtable and reserve-placement constraints):
`docs/AGENTS_ARCHIVE_2026-09-16.md`. Ordering after this per ADVISORY-002 §7: (iv) fd/HANDLE
indirection, then relaxing the `beyond_stdio` fork-eligibility gate.

## Closed — do not re-attempt without a genuinely new approach

**There is no open host crash** — the `RtlpUnwindPrologue` crash earlier notes called "the one genuinely
open" one was `VEH_FRAME_STRIDE`: a 4096-byte per-level slice 168 bytes short of the two frames it must
cover, nested by `fork_verify`'s own AV-heal storm. Bisected live: 10/10 fatal before, 0/10 then 0/57
after (two follow-up commits; mechanism: archive). Unguarded, not a live defect: PRD
`veh-frame-stride-has-no-overflow-guard`.

**Windows CoW-mmap performance**: zero practical effect on tar-packed execs (`MapViewOfFile3` needs 64KiB
file-offset alignment; ELF `PT_LOAD` segments are only page-aligned, no exploitable slack).
**`LITEBOX_COW_MMAP` default-off is load-bearing** — the shipped flank fix recommits orphaned flanks as
zero-fill; opting in trades a loud SIGSEGV for silently zeroed symbol tables
(`docs/cow-mmap-fixed-address-design.md`).

**Input latency**: three real bugs fixed and verified live (sub-pixel remainders now accumulated
losslessly; two evdev reports per move now one `SYN_REPORT`; window now resizable with scaled deltas).
Present mode is Mailbox-preferred with Fifo fallback — any note calling it Fifo-only is stale. Open PRD:
`mouse-motion-devicevent-needs-pixel-calibration`,
`linux-macos-userland-presentation-still-emits-two-syn-reports-per-move`; no framerate baseline exists
(idle compositor legitimately produces zero page flips).

**The GUI protocol decision is settled**: DRM/KMS + wgpu, proven live with guest page-flip pixels in a
real host window. Not an open X11-vs-Wayland-vs-DRM question.

## Presenter-process split (`docs/presenter-process-design.md`) -- done, fully verified live end-to-end, 2026-09-16

`litebox_presenter_protocol` crate + runner-side `ControlServer` (zero-copy scanout handoff) +
`litebox-presenter.exe` (new crate, zero shim/kernel dependency); `--gui` is now `Option<GuiMode>`.
Verified live across two sessions: every control-pipe command headless/`--gui=hidden`, spawn/respawn,
byte-identical `LITEBOX_DUMP_FRAMES` regression, kill-mid-display respawn-and-resume within ~370ms.
One real bug found and fixed: missing per-call `OVERLAPPED` made the first real `show` silently
`exit(0)` (fixed via `pipe::overlapped_call`; do NOT fix by removing `FILE_FLAG_OVERLAPPED` — tried,
deadlocks instead). Small disclosed pre-existing gaps unrelated to the crash (`frames on <dir>`,
`ps` PROCESS_TREE, `PrintWindow` partial-shape). Full narrative: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

## Docs and tooling map

- **Archives** — `docs/AGENTS_ARCHIVE_2026-09-16.md` (popup-menu re-test, `spawn_exec_collision_child`
  fix, Track A audit, RawMutex/presenter mechanism detail), `_2026-09-15.md` (ACK-stall-kill detail),
  `_2026-09-10.md` (fork fd eligibility, cost history, OCI cache, s6-boot, browser config, crash-dump/
  VEH, CoW, working practices). Older: `_2026-09-03.md`, `_2026-09-05.md`.
- Fork: `docs/track-b-fork-fix-progress.md`, `advisor/ADVISORY-002-d-zero-fork.md`,
  `advisor/ADVISORY-001-fundamentals.md` (§3N tcache, Appendix D presenter case).
  `docs/veh-exception-handler-design.md` — canonical VEH narrative, read before touching the handler.
- Desktop logs: `docs/webtop-debian-selkies-2026-09-06.md`, `webtop-alpine-mate-2026-09-07.md`,
  `webtop-debian-xfce-2026-09-08.md`, `webtop-xfce-code-vs-data-2026-09-08.md`, `fork-fs-veh-2026-09-08.md`.
- Consult before deriving: `docs/premade-library-research.md`, `docs/drm-dumb-buffer-ioctl-reference.md`
  (kernel UAPI for DRM syscalls), `docs/diag-timeline-field-semantics.md` (before any `DIAG_TIMELINE`
  `comm`-field hypothesis — two investigations mis-traced it).
- `docs/macos.md` — port state; the Apple Silicon guest-execution context switch is a stub, stays
  deferred (PRD `macos-aarch64-guest-execution-context-switch-is-not-implemented`,
  `gui-macos-presentation-runner-and-guest-entry-blocked`). Probe crates: `docs/wayland-drm-backend-probe/`,
  `docs/linux-native-drm-gui-probe/`.
- `docs/presenter-process-design.md` -- IMPLEMENTED and fully live-verified 2026-09-16; see this file's
  own "Presenter-process split" section above. Designs NOT implemented: `docs/session-daemon-design.md`
  (`litebox_termemu`'s VT100-emulator slice IS implemented; the daemon/IPC layer is not),
  `docs/fork-region-grouping-design.md` (still a diagnostic probe).
- `advisor/probes/` — diagnostics (`decode_frame.py`, `symbolize_litebox_crash.py`, `dup_probe.c`,
  `drm_flip_probe.c`, `clone_probe.c`) plus `MEASUREMENT-PITFALLS.md`, `DISK-HYGIENE.md`. OCI-pull
  Python scripts there are retired.
- `.gm/memories/`: `mem-c62454fedb1baef8-2714` (RtlpUnwindPrologue), `mem-e5107049137fcf43-1303`
  (browser witness), `mem-7cb09e839ca086f2-4223` (XFCE/MATE/weston), `mem-6c4697ac568ea7be-4487`
  (packager OOM), `mem-136ae2ce29bc28a4-3133` (image tags), `mem-b709a7d784b98110-1430` (cross-process
  sync), `mem-f17269d5777055d3-3326` (2026-09-07 defects), `mem-3e13872ce1ffe95e-2814` (CoW),
  `mem-3c4a9980a884604b-1031` (GUI protocol).
