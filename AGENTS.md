# litebox — current state (2026-09-07)

This file is the authoritative, CURRENT-STATE picture of what works, what's broken, and what to
do next — consolidated, not historical. It does not narrate how each conclusion was reached; it
states what is true right now. The full pass-by-pass forensic history (every investigation,
every retracted hypothesis, every dead end) is preserved in `docs/AGENTS_ARCHIVE_2026-09-03.md`
(up to 2026-09-03) and `docs/AGENTS_ARCHIVE_2026-09-05.md` (2026-09-03 through 2026-09-05) — read
those only if you need the detailed reasoning trail behind something stated here, not as a
starting point for new work.

This file is also the single source of truth for standing rules, hard constraints, and durable
lessons. Any future "remember this" belongs here, not in a separate memory file or a new pass
narrative appended to the bottom.

## Standing lessons and hard constraints (read before doing anything)

- **No WSL or hypervisor, ever, for anything litebox-related** (building, running, verifying) --
  litebox's whole premise is running unmodified Linux ELF binaries directly on bare Windows via
  syscall rewriting; reaching for WSL2/Hyper-V/any VM undermines that. Cross-compiling FOR Linux
  from any host is fine; RUNNING the result inside a VM/WSL is not -- always run it as a real
  litebox guest process via the matching runner (`litebox_runner_linux_on_windows_userland.exe`
  on Windows, `litebox_runner_linux_userland` on native Linux).
- **`fork_verify.rs` and its whole stale-pointer-healing bug class are Windows-only** -- real
  Linux/macOS `fork()` gives the child identical virtual addresses, so this bug class structurally
  cannot occur there. Never assume a `fork_verify`-attributed crash needs Linux/macOS work, never
  port a `fork_verify.rs` fix to another platform's crate.
- **Never enable `bcdedit /debug on`** without a real kernel debugger already attached and
  confirmed working first -- caused two genuine full-host freezes (hard power-cycle required)
  combined with litebox's exception-heavy workload.
- **For `--gui` visual verification, always use `LITEBOX_DUMP_FRAMES=1`** (numbered `.bmp` +
  non-black-pixel count to stderr), never Windows `PrintWindow`/`CopyFromScreen` (unreliable,
  interfered with by overlapping windows).
- **A pixel/non-black count never identifies WHO painted a frame.** Decode frame structure
  (`advisor/probes/decode_frame.py`) and correlate against `DIAG_TIMELINE execve` log lines
  showing the real argv0 that actually ran, before attributing rendered content to a specific
  component. Cost this project real time repeatedly: weston's own built-in panel was mistaken for
  XFCE's more than once, and real panel-shaped pixels were once attributed to `xfce4-panel` on an
  image that turned out to ship MATE (`xfdesktop`/`xfce4-panel` never existed in that layer).
- **Never run two full-stack litebox verifications concurrently on this host** (including across
  sessions/peers) -- they starve each other, and the failure (log truncated mid-line, no crash, no
  exit) is indistinguishable from a real hang or regression. Check
  `Get-Process litebox_runner_linux_on_windows_userland` (or `tasklist | grep litebox_runner`) and
  coordinate with any peer session before booting.
- **Never time litebox with one host process per datapoint** (a bare process spawn costs
  1.6-2.3s, dwarfing real per-exec differences) -- hold host-process count constant: run N
  iterations inside ONE guest process and take the delta, never compare separate runner
  invocations. Also hold host load constant. Establish a real noise floor (10+ repeated runs)
  before comparing anything -- this project measured a ~20-36% run-to-run spread that made
  several earlier single-shot "findings" retract.
- **`log_unsupported!`/refusal errno choice is part of the API contract, not incidental.** EPERM
  ("you may not") lets callers degrade gracefully; EINVAL/ENOSYS ("this is broken/unknown") makes
  them fail hard -- getting this wrong can break unrelated features entirely (a `clone()`
  namespace-flag EINVAL once silently broke ALL PNG/JPEG decoding via glycin's bwrap fallback).
  Report what's actually true, never a fake success or an overly-broad failure.
- **A panic in litebox on Windows used to become a bare host crash with no panic message** -- the
  exception handler carried every exception code (including the MSVC panic code) through a stack
  swap into a handler with no interest in it, overflowing the stack. Fixed by whitelisting the
  four codes the handler actually triages, which also now registers FIRST in the VEH chain (not
  last, as it always had been). **If you see an unexplained `0xC0000005` with no further detail,
  consider that something panicked** before assuming a memory bug.
- **Use `advisor/probes/symbolize_litebox_crash.py` on any host-side crash dump, and snapshot the
  `.exe` + `.pdb` next to the log when you start a run you might need to debug.** The ring dump's
  `rva=` values are only meaningful against the exact build that emitted them -- symbolizing
  against a rebuilt binary gives confident, plausible, completely wrong names. Only
  `is_in_guest=false` entries carry a real module address; GUEST rips' `rva` wraps into nonsense.
- **Always build general debug/observability tooling proactively while investigating**, not just
  enough to explain the current bug -- e.g. separating guest stdout from litebox's own log stream,
  capturing a component's own stderr instead of letting it get redirected to an unread file (a
  repeated blind spot: weston's, Xwayland's, and xfdesktop's stderr each went unread for a long
  stretch before someone checked).
- **Prefer premade, mature libraries over hand-rolled code for well-known problem classes** (OCI
  clients, binary-format parsing, unwind-info construction, crash/minidump handling, CLI parsing,
  serialization, etc.) -- research what already exists before writing more custom logic. See
  `docs/premade-library-research.md` for the audit findings so far. Only hand-roll when research
  confirms no good existing solution fits a genuinely litebox-specific constraint.
- **Isolate the harness before blaming litebox.** Launch guest test probes directly from their own
  minimal tar layer as the runner's top-level program, never through a runtime-built `/bin/sh -c`
  wrapper -- harness bugs (MSYS2 path-mangling, a shell SIGILL) have each produced a false
  "litebox is fundamentally broken" claim that disappeared once the harness variable was removed.
- **Build freestanding guest test binaries on the HOST**, not the guest toolchain (both the
  guest's clang and gcc are broken as of this writing) -- `clang --target=x86_64-unknown-linux-gnu
  -nostdlib -nostdinc -ffreestanding -fno-stack-protector -static -O1`, producing a static
  `ET_EXEC` with raw `syscall` instructions, no libc.
- **Inject a new probe/script into a multi-GB layer via a small `--resume-from` overlay tar**
  (just the new file, `tar cf overlay.tar -C <dir> file`), never by rebuilding the whole layer.
  Needs a real Windows path (not MSYS `/tmp/...`) and `MSYS2_ARG_CONV_EXCL='*'`/
  `MSYS_NO_PATHCONV=1` set, or it fails in two different misleading ways (an ENOENT that looks
  like a missing shebang resolver, or a stack-overflow panic).
- **`linuxserver/webtop:alpine-mate` ships MATE, not XFCE; `alpine-xfce` doesn't exist as a tag.**
  Use `debian-xfce`/`ubuntu-xfce` for real XFCE (see "Container images" below for the full tag
  survey). The MATE path's own remaining blocker is the unresolved `mate-session`
  `RtlpUnwindPrologue` crash (see below), not an xfconf/wallpaper gap.
- **Repo hygiene**: large binary artifacts (packed layer tars, frame dumps, debug logs) never
  belong in git -- keep them in `.wfgy/` (gitignored) or a durable-but-untracked sibling directory
  like `../litebox-webtop/`. Root-level scratch files (`probe_*.tar`, `*.bmp`, `*.log`) are
  gitignored; if `git add -A`/`git add .` sweeps one in by accident, untrack it rather than leave
  it committed.

## Webtop browser-verified video pipeline (WORKING, as of 2026-09-07)

**A real guest desktop stack (selkies, inside a `linuxserver/webtop` image) streams live, changing
video frames into a real browser on the host** -- confirmed via three consecutive browser
screenshots each a different color, matching a root-window painter cycling color once per second
inside the guest. The whole pipeline (Xvfb, X client, selkies/pixelflux x264, MIT-SHM capture)
runs inside litebox; only the reverse proxy is host-side. Full narrative, reproduction recipe, and
load-bearing gotchas: `docs/webtop-debian-selkies-2026-09-06.md` (Track A) and
`docs/webtop-alpine-mate-2026-09-07.md`.

Getting here required fixing seven real, independent litebox defects, all landed: `insert_mapping`'s
Hint-address ENOMEM, `BootLock`'s Drop-based release leak, Windows CoW-mmap library corruption,
`futex` PI-ops EINVAL aborting PulseAudio, unimplemented `sys_waitid`, `resize_mapping`'s
`unreachable!()` panic, and unimplemented SysV shared memory (the final blocker -- selkies'
pixelflux capture needs MIT-SHM). See memory: webtop-video-pipeline-seven-defects-fixed
(mem-f17269d5777055d3-3326) for the full per-defect root-cause writeup.

**Not yet done**: audio, clipboard, and gamepad remain disabled in this recipe because each is a
fork site that still risks the `fork_verify` host-side AV (the real architectural gap -- see
"Track B" and the `RtlpUnwindPrologue` section below). MATE itself is not running in the verified
alpine-mate repro, so the confirmed desktop content is a painted root window, not a full desktop
session.

## Track B: `D == 0` cross-process fork (fast now; a real, newly-reachable concurrency hang replaces the old perf blocker)

A genuine `D == 0` (child lands at the SAME addresses as the parent, no relocation, no
`fork_verify` healing needed at all) cross-process fork already exists in-tree
(`LITEBOX_PROCESS_FORK=1`, `spawn_cross_process_fork_child`) -- see `advisor/ADVISORY-002-d-zero-
fork.md` for the full feasibility case. It is hard-gated off for every real workload by one check
(`fd_complexity.beyond_stdio == 0` in `litebox_shim_linux/src/syscalls/process.rs`): any guest
holding an fd at or above 3 -- i.e. every XFCE component, every X client, every D-Bus participant
-- falls back to the existing thread-based relocating fork and its `fork_verify` healing, the
path that still carries ADVISORY-001 section 3N's tcache-corruption risk.

**Current verdict, 2026-09-10 (supersedes the "inconclusive, Defender scan-gating" verdict this
section used to carry): cross-process fork IS correctness-sound.** A minimal, fully isolated
repro (`bash -c` doing `x=$(echo hi)` in a loop) run with `LITEBOX_PROCESS_FORK=1` genuinely set,
confirmed actually taking the cross-process path via the `[process_fork_diag] task-resume-probe`
log trail: **zero corruption across every completed fork**, `$(...)` correctly captured `hi`
every time -- a stark contrast to the thread-based default's 100% `malloc(): unaligned tcache
chunk detected` crash rate on the identical repro. This directly confirms the mechanism
ADVISORY-002 predicts: no relocation, no stale-pointer hazard for `fork_verify` to (mis)heal.

**The real, now-measured blocker is performance, not correctness:** each fork costs roughly
3.5-5 SECONDS of overhead, even with every OCI layer at `[cache] HIT` (no network, no
re-rewriting) -- spent re-deriving the full in-memory rootfs (`pull_layers_in_memory` re-reading
and re-merging ~17 cached layers of a multi-GB image) and cold-starting a fresh
`WindowsUserland::new()` (VEH registration, console-watcher thread, NAT gateway `net_worker`) on
every single fork, from scratch, even though the result is byte-identical every time within one
run. A real XFCE boot forks dozens to low hundreds of times; at this cost that is minutes to
hours of avoidable overhead -- almost certainly the real explanation for this investigation's own
earlier "15 real minutes, 7 guest-seconds of progress" full-webtop-stack observation under
`LITEBOX_PROCESS_FORK=1`, previously (and, in light of this, likely wrongly) attributed to a
Defender scan-gate or a pathological `fork_verify` healing loop.

**2026-09-10, later: the performance blocker above is FIXED (commit `ce5648f`).** The per-fork
cost was never `WindowsUserland::new()`/rootfs re-merge overhead as first suspected -- it was
`fork_verify::is_readable` calling `VirtualQuery` once PER 4KB PAGE on the parent side while
copying the child's memory (`VirtualQuery`'s cost scales with total committed memory, a VAD-tree
walk). Caching the queried region's bounds across consecutive pages (`fork_verify::
readable_region`) cut a 173MB group's copy time from 23.5-25.4s to 400-650ms (**~40-60x**). The
real `webtop_stack.sh` boot under `LITEBOX_PROCESS_FORK=1` now reaches `NGINX_CONFIGURED`/
`NGINX_STARTED` in under a minute, versus never getting there in 15+ minutes before. Full
measurement/rejected-alternative narrative: `docs/track-b-fork-fix-progress.md`.

**This performance fix immediately exposed a real, previously-unreachable correctness bug, now
ROOT-CAUSED AND FIXED (commit `060ccc3`): two backgrounded cross-process forks from the SAME
parent thread, followed by `wait`, hung the parent forever.** Root cause was NOT the
concurrent-corruption class this section used to guess at -- it was a missing `SIGCHLD` bridge.
A `LITEBOX_PROCESS_FORK=1` child is a genuinely separate Windows process reconstructing its own
`Process` from scratch, with no `Arc` back to the real parent -- confirmed via `Process::
prepare_for_exit`'s own `has_live_parent` gate reading unconditionally `false` for such a child,
silently skipping the SAME same-process `SIGCHLD`-delivery step a thread-based child's exit
already uses. A parent blocked the race-free way (mask `SIGCHLD`, `sigsuspend`/`pause` to
atomically wait for it -- busybox ash's plain `wait` builtin does exactly this with more than one
backgrounded job) therefore hung forever the moment it had any cross-process-fork child, even
after that child had already exited: nothing was ever going to wake it. Pinpointed via debug
syscall tracing against `advisor/probes/cross_process_fork_wait_hang_probe.sh`: the parent's last
syscall ever was a non-blocking `sys_wait4(WNOHANG)` (correctly returns `Ok(0)`) immediately
followed by `rt_sigprocmask`, then silence -- the standard mask-then-sigsuspend idiom's second
half never got traced because `sys_pause`/`sys_rt_sigsuspend` have no entry log, not because
nothing happened.

Fix: new `ForkChildVerificationProvider::spawn_cross_process_exit_notifier` (`litebox/src/
platform/mod.rs`), Windows-implemented via a spawned thread blocking on the existing
`wait_for_cross_process_exit`, wired into both real `do_clone` cross-process-fork sites
(`litebox_shim_linux/src/syscalls/process.rs`) to push the child's `exit_signal` into the
parent's `shared_pending` and call `interrupt_all_threads()` on exit -- exactly mirroring
`prepare_for_exit`'s existing same-process notify step.

**A THIRD real bug, immediately downstream of fixing the second, also root-caused and fixed
(commit `6e86a40`): `sys_wait4(pid=-1)` only consulted `cross_process_children` when `children`
(thread-based) was ALREADY empty at call time** -- backwards for the common shape where a shell
forks several plain commands (`mkdir`/`cp`/`sed`, thread-based) before backgrounding a LATER
cross-process fork, leaving `children` non-empty and the cross-process registry never checked by
the blocking wait loop. `poll_once` (shared by the `WNOHANG` and blocking paths) now checks
`cross_process_children` first on every invocation, not just once up front.

**Net result, verified live:** the minimal repro (`advisor/probes/cross_process_fork_wait_hang_
probe.sh`) and a 148-line truncation of the real `webtop_stack.sh` (everything through its nginx
self-test) both complete cleanly and deterministically; existing correctness repros
(`bashfork_repro.sh`) still show zero corruption; the FULL `webtop_stack.sh` now correctly falls
through its nginx self-test instead of hanging there. **Not yet fixed:** nginx's own SSL-cert
generation failure on its real first startup attempt (the ORIGINAL symptom this whole
investigation started from) -- still reproduces, not yet root-caused. `webtop_stack.sh`'s own
comments separately note `LITEBOX_PROCESS_FORK=1` as "documented unreliable"/breaking Xvfb/dbus,
a pre-existing caveat not re-tested against these three fixes. Host note: each `litebox_runner`
process under this OCI image holds 650MB-1GB+ resident; this host has limited free RAM (seen as
low as ~800MB) -- always kill every `litebox_runner` process between test runs, never run two
concurrently (see "Never run two full-stack litebox verifications concurrently" above). Full
narrative, exact measurements, and rejected alternatives for all three fixes:
`docs/track-b-fork-fix-progress.md`.

## Container images

**Use `litebox_packager --oci-image <ref> --output <tar>` to pull and package any Docker/OCI
image.** This is a complete, working, already-tested Rust tool (`oci-client` + `oci-spec` +
`litebox_syscall_rewriter`) that pulls the image, correctly handles whiteout/opaque-whiteout
files, rewrites every ELF, and produces a bootable tar in one command. It supersedes every
ad-hoc Python script this project previously hand-rolled for the same purpose (`pull_oci_image.py`,
`batch_rewrite_layer.py`, `fetch_container.py` -- all retired, do not recreate them).

**`linuxserver/webtop:alpine-mate` ships MATE, not XFCE** (see standing lessons above).
`linuxserver/webtop:alpine-xfce` does NOT exist (404) -- do not pull it. `debian-xfce`/`ubuntu-xfce`
DO ship a real XFCE stack (confirmed live, correcting an earlier truncated-pull false negative).
**Always verify a desktop image's actual WM/session binaries by direct registry manifest + blob
tar-listing, or a live `/usr/bin` listing inside a fully booted guest, BEFORE assuming either way**
-- never trust the tag name. See memory: container-images-debian-xfce-tag-verification-and-oci-in-memory-loading-history
(mem-136ae2ce29bc28a4-3133) for the full verification trail.

**Runtime OCI images are structurally the wrong shape for this project's actual DRM device.**
`litebox`'s virtual DRM device is legacy-KMS + dumb-buffer + XRGB8888 only (no atomic modeset, no
GBM/EGL) -- a modern Wayland/GBM-first compositor is pushed onto its least-tested
software-rendering fallback path. **Use `Xorg` with the `modesetting` driver, never `Xvfb`**
(`Xvfb` never touches DRM/KMS at all -- zero page-flips, zero frames, indistinguishable from "the
guest never drew"). Also: launch the target binary directly as the runner's top-level program
(`--oci-image ... -- /usr/bin/Xorg :0 ...`), bypassing the container's own s6-overlay/selkies
entrypoint -- its init pulls in Python, WebRTC, and process supervision that contribute zero
pixels.

**Runtime, in-memory OCI loading (implemented, working)**: `litebox_runner_linux_on_windows_userland
--oci-image <ref>` pulls, merges, and syscall-rewrites every layer entirely in memory -- no real
host directory is ever created for the rootfs (extracting to a real directory was the original,
now-abandoned approach; it hit three independent Windows-path bugs). Verified live:
`docker.io/library/busybox:latest` pulled, merged, rewritten, booted, real guest output. The
ahead-of-time `litebox_packager --oci-image <ref> --output <tar>` path still exists unchanged for
callers that want a pre-built, reusable flat tar (full history: same memory key as above).

**Perf**: `tar_ro.rs`'s multi-layer path index is built ONCE at mount time, not re-scanned per
guest file read. **Memory**: pulling a large image's largest layer can OOM -- four independent
`litebox_packager` buffer-sizing/streaming bugs were found and fixed, and a remaining constraint
was confirmed to be host-memory-watchdog contention from unrelated processes, NOT a litebox bug
(a small image like `busybox:latest` remains reliable regardless of host load). Every
`linuxserver/webtop:*` flavor shares the same ~519MB blocking base layer, so switching desktop
flavor within the webtop family does not avoid this. See memory:
litebox-packager-oom-four-bugs-and-host-memory-exhaustion-nonbug (mem-6c4697ac568ea7be-4487).

**Canonical layer, older hand-assembled weston/XFCE path**: `.wfgy/xfce-build/layer31_direct_fixed.tar`
(superseded in priority by the stock-image path above). **Durable copy of the stock MATE webtop
image**: `C:\dev\litebox-webtop\webtop_seatd.tar` -- what the `mate-session` crash investigation
below reproduces against.

## XFCE/MATE desktop status

**Canonical hand-assembled layer (`layer31_direct_fixed.tar`, weston + XFCE)**: a real XFCE
session comes up and stays alive -- `weston`, `xfwm4`, `xfconfd`, `xfsettingsd`, `xfdesktop`,
`xfce4-panel` all run without crashing, and `xfce4-panel`'s clock genuinely ticks. Real cross-thread
signal delivery, repeating-timer (`sys_setitimer`), and `gschemas.compiled` bugs were found and
fixed along the way.

**Real, unresolved gap on this same layer**: the desktop background (not just the panel) renders
inconsistently across runs -- investigated extensively (shared memory, DRM buffer corruption both
ruled out) but not root-caused; a real non-deterministic race, not one deterministic bug.

**Real, unresolved gap, separately**: `gdk-pixbuf`/GTK image decoding was investigated at length.
PNG/JPEG decode correctly; SVG has no loader shipped (packaging gap, not a litebox bug); XPM fails
specifically when read from litebox's tar-RO filesystem backend combined with its dlopen'd loader
module (narrow, litebox-facing, not yet root-caused).

**Stock MATE webtop image (`webtop_seatd.tar`)**: real DRM/wgpu rendering confirmed working via
`labwc`. `TEST_DONE` reached cleanly with a real panel confirmed rendering. Further progress
(MATE-native session, actual desktop content) is blocked by the `RtlpUnwindPrologue` crash below,
since `mate-session`'s own launch sequence is exactly the shape that triggers it.

See memory: xfce-mate-desktop-weston-fixes-and-rendering-decoding-investigations
(mem-7cb09e839ca086f2-4223) for the full fix/investigation detail behind all four paragraphs above.

**Stock XFCE webtop image**: see "Container images" above (`debian-xfce`/`ubuntu-xfce` preferred,
`alpine-xfce` does not exist, `arch-xfce` deprioritized) -- check `git log` for the latest status
before assuming a result either way.

## The `RtlpUnwindPrologue` crash (genuinely unresolved, do not attempt a fix without new evidence)

A real, host-side (not guest-code, not litebox's syscall-emulation logic) Windows platform bug.
Deterministically triggered by 3 consecutive execs of a large binary (`mate-session --version`
x3 against `webtop_seatd.tar` is the exact, reproducible repro -- no compositor needed, ~90s).
30+ archived investigation passes (`docs/AGENTS_ARCHIVE_2026-09-03.md`) plus several fresh
attempts -- multiple root-cause theories raised and retracted as better evidence arrived. Most
recently, a claimed `is_in_guest=true, addr=usize::MAX` first fault is itself now suspect: a later
154-event live capture showed `is_in_guest=false, addr=0x2` on every single event, the opposite of
that claim. Treat any specific prior claim in this area as unverified until re-examined against
fresh evidence.

**Concrete next step, not yet attempted and cheaper than the alternatives**: capture depth-0
(`is_in_guest=true`) via the existing allocation-free `RECENT_FAULTS` ring (`lib.rs:587`) or
`diag_raw_regdump`, against the `mate-session --version` x3 repro -- **do NOT run this under
`LITEBOX_VEH_TRACE=1`** (tracing's own overhead dodges the race this bug depends on; ~12
consecutive traced runs never reproduced it). The `AddVectoredExceptionHandler` registration-order
question this section used to raise is ANSWERED and fixed (now registers FIRST, correctly) -- note
this also means litebox no longer sees Rust panics as panics, which used to produce a bare
`0xC0000005` easily mistaken for this bug. Do not guess at a fix without first getting real
evidence from the depth-0 capture -- this bug has already produced multiple retracted theories
from acting on incomplete understanding, and the explicit standard for this bug is a genuinely
root-caused fix, not one that merely stops the observed symptom.

Full investigation history (every retracted theory, the evidence that retracted it, and the two
lower-priority next-step options): memory rtlp-unwind-prologue-crash-full-investigation-history
(mem-5ad34546c566b8d6-7306).

## Windows CoW-mmap performance (investigated thoroughly, not worth pursuing further)

`try_allocate_cow_pages` is implemented for Windows but has **zero practical effect** on real
tar-packed execs -- root cause conclusively established and closed: Windows' `MapViewOfFile3`
needs 64KiB file-offset alignment, but real ELF `PT_LOAD` segments are only page-aligned with no
exploitable slack, and this is true regardless of tar-file-start alignment too. **Do not
re-attempt without a genuinely new approach.** Separately, a real correctness bug in this same
path was found and fixed (2026-09-07): a Windows `MAP_FIXED` sub-range remap could destroy and
zero-fill a shared library's flanking memory, silently corrupting it -- the path is now opt-in
only (`LITEBOX_COW_MMAP`, default off), with the flank-restoration logic itself also fixed. See
`docs/cow-mmap-fixed-address-design.md` and memory:
windows-cow-mmap-performance-closed-negative-result-plus-correctness-fix
(mem-3e13872ce1ffe95e-2814) for the full measurement/rejected-alternative detail.

## Input latency (fixed, shipped)

Three real bugs found and fixed in the Windows presentation/input path, all verified live: slow
mouse movement was silently dropped (float-to-i32 truncation discarded sub-pixel remainders,
now accumulated losslessly); every physical mouse movement was delivered as TWO separate evdev
reports instead of one grouped report (now one `SYN_REPORT` per physical movement); and the
window was locked to the guest's fixed virtual resolution (now resizable, with mouse deltas
scaled by the visible-guest-to-window-pixel ratio so cursor tracking stays correct at any size).

Not yet done: no framerate baseline exists (an idle compositor with no client legitimately
produces zero page flips, so there's nothing to measure against yet) -- needs a real, moving,
on-screen client first.

## Repo/tooling notes

- `docs/premade-library-research.md` -- the ongoing library-vs-hand-rolled-code audit (see the
  standing lesson above). Check it before writing new infrastructure code in any of the areas it
  covers.
- `advisor/probes/` holds various diagnostic scripts and probes accumulated across this
  investigation (`decode_frame.py`, `run_xfce_xwm.sh` and variants, `drm_flip_probe.c`, etc.) --
  useful, keep using them, but don't assume every script there is still the current recommended
  path (e.g. the OCI-pull Python scripts are retired, see "Container images" above).
- **Test-suite status (2026-09-09, measured): everything is green.** `cargo test -p litebox --lib`
  150/150, `-p litebox_shim_linux --lib` 187/187, `-p litebox_platform_windows_userland`,
  `-p litebox_common_linux` and `-p litebox_syscall_rewriter` all clean. Getting there fixed real
  defects (a crash hiding most of `litebox_shim_linux`'s suite, two genuine `litebox --lib` logic
  bugs mislabeled as `diod`-environmental) that had been sitting unexamined behind a misreported
  count and a permanently-red-for-environmental-reasons suite. **Never record a test count you did
  not just watch run to completion, and never leave a suite red for an environmental reason.**
  (Supersedes any older "26 failing, 9 need `diod`" note dated before 2026-09-09.) Full history:
  memory test-suite-status-history-2026-09-09-miscounting-root-causes (mem-14fccd59cddec385-2382).
- `docs/webtop-debian-selkies-2026-09-06.md` (Track A) and `docs/webtop-alpine-mate-2026-09-07.md`
  -- the full, dated investigation logs behind "Webtop browser-verified video pipeline" above.
- `docs/track-b-fork-fix-progress.md` and `advisor/ADVISORY-002-d-zero-fork.md` -- the full log
  and design case behind "Track B: `D == 0` cross-process fork" above, including the 2026-09-10
  `CLAIMED_RANGES` cross-process-collision root-cause fix for Xvfb's deterministic crash.
- `docs/presenter-process-design.md` -- design spec (not yet implemented) for moving the
  window/wgpu/winit presentation loop out of the same process as guest syscall emulation, per
  `advisor/ADVISORY-001-fundamentals.md` Appendix D.
- `docs/session-daemon-design.md` -- design for agent-driven multi-session TTY control; its
  foundational slice (`litebox_termemu`, a pure bytes -> rendered-screen VT100 emulator) is
  already implemented, the daemon/IPC layer on top of it is not yet built.
- `docs/fork-region-grouping-design.md` -- permanent-fix design (not yet implemented; the
  shipped state is still a diagnostic probe) for provenance-based region grouping in
  `Vmem::duplicate`, replacing the current gap-heuristic grouping.
- `docs/drm-dumb-buffer-ioctl-reference.md` -- verbatim kernel UAPI struct reference for the
  dumb-buffer DRM/KMS ioctls `litebox_shim_linux/src/syscalls/drm.rs` implements against; consult
  it rather than re-deriving struct layouts from scratch when touching that file.
- `docs/diag-timeline-field-semantics.md` -- read before building any new hypothesis on
  `DIAG_TIMELINE`'s `comm` field; two separate investigations already mis-traced it once.
