# litebox — current state (2026-09-16)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, and a claim a later commit superseded, were deleted rather than hedged or kept
for their story. Reference detail is drained to the `docs/AGENTS_ARCHIVE_*.md` files and
per-investigation logs to the dated `docs/*.md` in the map below — read those for a trail, never as a
starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one line
plus its pointer, not in a separate memory file and not as a pass narrative appended below.

## The cheap repro — start here

```
target/release/litebox_runner_linux_on_windows_userland.exe -Z \
  --oci-image docker.io/library/debian:stable-slim -- /bin/bash -c '<script>'
```

One ~81MB layer, `[cache] HIT` after the first pull, real GNU coreutils instead of busybox — which
matters: coreutils `touch` issues the `utimensat(fd, NULL, …)`/futimens form busybox's never reaches
(`caaac79`). Two host-side gotchas, each already costly:

- **PowerShell, never Git Bash** — Git Bash rewrites `/absolute/guest/paths` into
  `C:/Program Files/Git/...` before the runner sees them, giving a misleading `ENOENT` on the program
  path. **`Start-Process -RedirectStandardOutput/-RedirectStandardError` makes the runner exit almost
  instantly with zero guest output** (no crash dump, no event-log entry — its console-handle expectations,
  ADVISORY-002 §3.1, aren't met by that redirection shape); use `& .\runner.exe ... *> combined.log`
  instead, confirmed live to run normally.
- **Single quotes only inside `-c`** — embedded double quotes are corrupted crossing into the child's
  Win32 command line. This masqueraded as deep fork/stack-pointer corruption for a whole sub-session.

**Log level**: the default is `warn,litebox_platform_windows_userland::fork_verify=error`
(`litebox_runner_linux_on_windows_userland/src/lib.rs:499`, `e96c4e5` — `EnvFilter`'s own ERROR-only
default discarded all 111 real `warn!` sites; `fork_verify` is pinned to `error` because it warns per
single-stepped instruction). Do **not** add `LITEBOX_LOG=error` by reflex; use `fork_verify=warn` when a
fork heal is the subject.

## Standing lessons and hard constraints

- **No WSL or hypervisor, ever.** Cross-compiling FOR Linux is fine; RUNNING the result in a VM defeats
  the whole premise — always run it under the matching runner
  (`litebox_runner_linux_on_windows_userland.exe`, or `litebox_runner_linux_userland` on Linux).
- **`fork_verify.rs` and its stale-pointer-healing bug class are Windows-only** — real `fork()` gives the
  child identical addresses, so the class cannot occur elsewhere. Never port such a fix to another
  platform's crate.
- **Never `bcdedit /debug on`** without a kernel debugger already attached and confirmed: two full-host
  freezes needing a power-cycle, against litebox's exception-heavy workload.
- **Never run two full-stack verifications concurrently**, peer sessions included — they starve each
  other, and the failure (log truncated mid-line, no crash, no exit) is indistinguishable from a real
  hang. Each runner under an OCI desktop image holds 650MB-1GB+ resident against ~800MB free on this
  host; kill every `litebox_runner` between runs. **Free RAM has been less stable than that baseline
  implies** (2026-09-16: one `debian-xfce`+selkies boot's RSS passed 4.5GB by `DE_UP` alone, host starting
  at ~6GB free) — watch `FreePhysicalMemory` live and kill on a falling trend, not a fixed RSS number.
- **`LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check** (numbered `.bmp` +
  non-black-pixel count to stderr), never `PrintWindow`/`CopyFromScreen`.
- **A pixel count never identifies WHO painted a frame** — decode frame structure
  (`advisor/probes/decode_frame.py`) and correlate against `DIAG_TIMELINE execve`'s real argv0. Cost real
  time twice: weston's panel mistaken for XFCE's, panel-shaped pixels attributed to `xfce4-panel` on a
  MATE image that has none.
- **Never time litebox with one host process per datapoint** — a bare spawn costs 1.6-2.3s, dwarfing real
  per-exec differences. Run N iterations inside ONE guest process and establish a noise floor (10+ runs):
  this host's ~20-36% spread retracted several single-shot "findings". Never subtract timestamps across a
  parent log and a fork-child log — every child's `init_logging()` resets elapsed time to ~0 (`4b95600`).
- **Refusal errno choice is API contract.** EPERM lets callers degrade; EINVAL/ENOSYS fails them hard. A
  `clone()` namespace-flag EINVAL once silently broke ALL PNG/JPEG decode via glycin's bwrap fallback, and
  an unknown-socket-option EINVAL broke GdkPixbuf→glycin→D-Bus decode for every format (`694bb93`, now
  `ENOPROTOOPT`). `EOPNOTSUPP` is itself fatal on glibc (`futex_fatal_error()`) — killed selkies one line
  after the cursor was delivered (`a120c56`).
- **Proving a run took the cross-process fork path needs `[process_fork_diag] task-resume-probe` lines,
  never the shim's eligibility log** — the shim logs `clone: cross-process fork() is eligible`
  (`litebox_shim_linux/src/syscalls/process.rs:2807`) regardless of whether the fork actually happened
  that way. This trap produced two recorded false conclusions (archive).
- **fork carries pipes, regular files and the writable layer into a child, but NOT sockets** — so
  `dbus-daemon --fork` serves nothing: the socket is created pre-fork, the child accepts on nothing, and
  the client's `connect()` still succeeds against the filesystem path then hangs in `ppoll(timeout=-1)`.
  Run dbus-daemon non-forking — this one fact was the entire MATE black screen (`181ec68`). For XFCE use
  `xfce4-session`, never `startxfce4` (that starts a second bus on a different address).
- **Use `advisor/probes/symbolize_litebox_crash.py` on host-side crashes, and snapshot `.exe` + `.pdb`
  next to the log.** A ring dump's `rva=` is meaningful only against the exact emitting build;
  symbolizing against a rebuild gives confident, plausible, wrong names. Only `is_in_guest=false` entries
  carry a real module address.
- **Isolate the harness before blaming litebox** — launch guest probes directly as the runner's top-level
  program from their own minimal tar, never via a runtime-built `/bin/sh -c` wrapper. MSYS2 path mangling
  and a shell SIGILL each produced a false "litebox is fundamentally broken" claim.
- **Never trust a container tag name for its WM/session contents** — verify by registry manifest + blob
  tar-listing, or a live in-guest `/usr/bin` listing. Earned three times.
- Procedural know-how is in the archive's "Working practices": building freestanding guest binaries on the
  HOST (both guest compilers are broken), injecting a probe via a small `--resume-from` overlay tar, and
  preferring mature libraries over hand-rolled code for known problem classes.
- **Never record a test count you did not just watch run to completion, and never leave a suite red for
  an environmental reason.** No counts are recorded here on purpose — a suite is not evidence of anything.
- **Repo hygiene** — packed layer tars, frame dumps and debug logs never go in git; keep in `.wfgy/`
  (gitignored) or an untracked sibling. Root-level scratch (`probe_*.tar`, `*.bmp`, `*.log`) is
  gitignored; if `git add -A` sweeps one in, untrack it.

## Guest-reachable code returns an errno, never a panic

The host process IS the entire guest session, so an `unimplemented!()`/`unreachable!()`/panic, or an
unbounded recursion, on any guest-reachable path kills every guest process at once. Bitten many times;
the fix is always the same shape — report what is true, as an errno, never a panic. All fixed, same
class: `6e14b71`/`2ce9ba8` (`layered.rs` metadata ops on lower-only dirs/chardevs), `133d3d4` (`O_NOATIME`,
`in_mem.rs:282`), `62e3c79`/`44189c7` (OOM → `ENOMEM` not panic), `ab2393d` (`listen(backlog=0)`/
re-listen), `a473048` (a guest open flag), `8aa05af` (a corrupted guest context), `5ec1ee4` (a debug-only
diagnostic), `1e1da7c` (nested `epoll_ctl(EPOLL_CTL_ADD)`, which calloop does routinely). Mechanisms:
archive.

## Cross-process fork (`LITEBOX_PROCESS_FORK=1`)

A genuine `D == 0` fork — child at the SAME addresses, no relocation, no `fork_verify` healing — exists as
`spawn_cross_process_fork_child` (design case `advisor/ADVISORY-002-d-zero-fork.md`).
`try_cross_process_fork` (`litebox_shim_linux/src/syscalls/process.rs:2524-2814`) short-circuits to a
native fork when `platform.has_native_fork()` — the whole fd-carrying apparatus is Windows-only
scaffolding for a missing syscall.

**It is correctness-sound** (`71a4bcd`): a minimal repro (`bash -c` doing `x=$(echo hi)` in a loop) shows
zero corruption across every completed fork, against the thread-based default's 100% `malloc(): unaligned
tcache chunk detected` rate on the identical repro. ADVISORY-001 §3N's tcache corruption is the
**thread-based** path's defect only. The `fd_complexity.beyond_stdio == 0` check older notes call the
gate is not one — `b2c89fc` found it emitting something false 67 times a boot.

**Eligibility** — refused only for `comm` == `Xvfb`/`dbus-daemon` (`process.rs:2560`, `564af3f` — a live
unix listening socket can't be served from a fork-time filesystem snapshot), an already-borrowed fd
table, a beyond-stdio fd that isn't a pipe end/path-recorded regular file/eventfd/close-on-exec
(overridable by `LITEBOX_PROCESS_FORK_IGNORE_FDS`), or an unsanitizable `fs_base`/context. On a real
`debian-xfce` boot the only remaining blocking kind is `unix-socket` — 5 refused forks of 34, down from
34/34 (`ed74c28`). Per-kind deviations (file offsets/eventfd counters copied not shared; CLOEXEC fds
dropped, so a pre-`execve` use gets `EBADF`): archive — read before assuming real `fork()` semantics.

**Per-fork cost** was ~3.5-5s, now ~1.2s (`1199ab6`, `cc7986f`, `ce5648f`); a full `webtop_stack.sh` boot
reaches `NGINX_STARTED` in under a minute versus never in 15+. The rootfs-re-merge/`WindowsUserland::new`/
writable-layer-growth explanations older notes give are **measured wrong**, ruled out by name (`78040e3`,
`6d4248b`/`c208e12`). Use `LITEBOX_DIAG_FORK_TIMING=1` for the next cost question. Three correctness bugs
the perf work exposed, all fixed: `060ccc3` (no `SIGCHLD` reached a cross-process child's parent),
`6e86a40` (`sys_wait4(pid=-1)` skipped the cross-process registry), `d5cc744` (a redundant claim release
deleted a coalesced `CLAIMED_RANGES` slot); mechanisms/repros/cost history: archive.

**Reading a cross-process log** — the `fork_verify` "stale CODE pointer" noise-vs-signal read is archived
(`docs/AGENTS_ARCHIVE_2026-09-15.md`).

**Still open**: nginx's own SSL-cert generation fails on its first real startup attempt — the original
symptom this investigation began from, genuinely not root-caused
(`docs/track-b-fork-fix-progress.md:146-152`). Do not cite `1f30ab4` as live open work: that was the
separate curl-self-test stall, fixed by `6e86a40`.

## Container images and OCI loading

**`litebox_packager --oci-image <ref> --output <tar>`** pulls, whiteout-merges, rewrites every ELF and
produces a bootable flat tar in one command (`litebox_packager/src/lib.rs:43-51`; x86-64/Apple Silicon
hosts only, `:114`) — supersedes the ad-hoc OCI-pull Python scripts this project once hand-rolled,
retired, do not recreate.

**Runtime in-memory loading** — `--oci-image <ref>` pulls, merges and rewrites every layer in memory; no
host directory is ever created for the rootfs (extracting to a real one hit three independent
Windows-path bugs). `litebox_runner_linux_on_windows_userland/src/lib.rs:94` (mutually exclusive with
`--initial-files`), `:630-638`, `:1350` (fork-child re-derivation). Rewritten layers are cached under
`.litebox-cache/` keyed on `(layer digest, REWRITER_CACHE_VERSION)`, so a rewriter change
self-invalidates. Large images pack fine now (`alpine-mate` 2937.9MB/59,067 entries; `ubuntu-xfce`
119,692 entries/13.7GB); residual risk is host-memory contention from unrelated processes, not a litebox
bug. `tar_ro.rs`'s multi-layer index is built ONCE at mount (`litebox/src/fs/tar_ro.rs:61,75-80`), not
per read — that build was O(entries²) and starting one process against the 2.5GB webtop rootfs went
17.3s → 0.35s (`90c010a`). Cache internals and the four fixed OOM bugs: archive.

**A trampoline-extension failure used to poison a whole segment's syscalls, now fixed** (`6311f74`). A
one-page initial allocation guess meant a segment needing more stub space (ordinary for a real binary)
extended at one fixed adjacent address with no fallback; any unrelated mapping there made
`apply_trap_fallback` poison **every** syscall in the segment with `ICEBP;HLT` on first use. Now sized
from a cheap `0F 05` byte-pair count (sound upper bound), capped at 4MiB. Witnessed live: `edgelevel/
alpine-xfce-vnc:latest` SIGILL'd within 3s before, zero fatal signals after. Blow-by-blow: archive.

**Tags, verified live, never from the name**: `linuxserver/webtop:alpine-mate` ships MATE, not XFCE;
`alpine-xfce` does not exist (404); `debian-xfce`/`ubuntu-xfce` DO ship real XFCE (`34da133`, `c65ab93`,
`1ea5203`; only the debian/ubuntu/fedora/arch bases carry it, `8c07f51`). `alpine-*` flavors share one
~519MB base layer (`9c7ea2b`); `debian-xfce` is a 17-layer Debian 13 image sharing nothing with them.
`edgelevel/alpine-xfce-vnc` is Alpine 3.16.0, Xvfb/browser pipeline (archive). `ubuntu-xfce` packs fine
but its rust-coreutils aborted in rustix auxv handling (`sleep`/`tail`/DE launch) — `bb46f1a` has since
implemented `/proc/self/auxv`/`AT_EXECFN`, so that's a re-test, not a fresh investigation.

**X server choice**: for the DRM/wgpu on-screen (`--gui`) path use `Xorg` with `modesetting` — litebox's
virtual DRM device is legacy-KMS + dumb-buffer + XRGB8888 only, no atomic modeset/GBM/EGL, so a GBM-first
compositor lands on its least-tested software fallback, and `Xvfb` never touches DRM/KMS at all (zero
page-flips, indistinguishable from "never drew"). For browser/selkies, `Xvfb` IS correct and verified:
`alpine-mate`'s `svc-xorg/run` execs `/usr/bin/Xvfb` (`Xorg` there is a 275-byte unused sh wrapper), and
its `-shmem` framebuffer works now that SysV shared memory exists (`4abf971`).

**Durable artifacts**: `C:\dev\litebox-webtop\webtop_seatd.tar` (stock MATE webtop);
`.wfgy/xfce-build/layer31_direct_fixed.tar` (hand-assembled weston+XFCE, superseded by the stock-image
path).

## A real desktop renders in a browser

**XFCE renders in a real host browser** (`f10c5e9`) and **MATE too** (`8c07f51`, `181ec68`) — full
pipeline (Xvfb, selkies/pixelflux x264, MIT-SHM) inside litebox, only the reverse proxy host-side.
Working config: selkies `--addr=0.0.0.0` port **8081**, dashboard over `--publish`, `/websockets`
tunnelled to 8081 (archive). Two sets of seven independent litebox defects got here, all landed (memory
`mem-f17269d5777055d3-3326`; 2026-09-08 set in `docs/AGENTS_ARCHIVE_2026-09-10.md`).

**A stock s6-overlay image boots with no flags/stubs**: `/init` on `webtop_seatd.tar` runs 16
cross-process children with zero uncarriable fds into supervision (`66bd640`) — retires three
"fundamental blocker" claims older notes carried.

**The black XFCE desktop was deterministic, now fixed** (`c7a1fa8`): the runtime rewriter corrupted
`libLLVM.so.19.1`'s `.dynsym`, so mesa `dlopen` failed forever and `xfwm4` never made a window. Not a
non-deterministic race.

Settled in the archive: PI futexes work; labwc's `wlr_swapchain_create` SIGABRT is upstream wlroots, not
litebox; every pre-`694bb93` gdk-pixbuf finding is stale (that fix broke-then-fixed decode for every
format); two network fixes this path needed (`537c088`, `2197d18`).

**XFCE also renders on the THREAD-based fork path, gated by one flag** (`docker.io/linuxserver/
webtop:debian-xfce`, `.wfgy/webtop_stack.sh`). Without it, 3/3 boots die ~7s in — ADVISORY-001 §3N's
safe-linked-tcache write, byte-for-byte (the `fork_verify: stale CODE pointer` warning right before it is
a red herring; translation is correct). Fix: `--env
GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0` (must be an `--env` runner flag — a
bare host-shell prefix does NOT reach the guest, confirmed live). Glibc-only workaround, not a fix (PRD
`glibc-tunables-workaround-pending-zero-fork`); `LITEBOX_PROCESS_FORK=1` removes the class properly but
can't run a desktop until the AF_UNIX gap below closes. Selkies also needs `--clipboard-enabled=false`
(its xclip-polling clipboard monitor re-triggers the same corruption every tick). Live witness: memory
`mem-e5107049137fcf43-1303`.

**Open here.** One client per selkies instance, no slot reclaim on reload. An intermittent host AV ends
some runs (`rip == fault address == 0x7ff003444000`, host-allocator region) — separate non-determinism
from the ACK-stall-kill below. Architectural gap: **guest processes share no AF_UNIX/loopback/FIFO
namespace**, so a cross-process fork gives zero AVs but Xvfb is unreachable from its own clients — one
shared host-side transport would put the whole desktop on the crash-free path
(`docs/fork-fs-veh-2026-09-08.md:128-144`).

**The glibc/tcache crash class still sporadically hits selkies**, separately from the ACK-stall-kill:
live-captured once, `gst_app_resize`'s xfconf-query DPI fork on a client's 5th rapid reconnect SIGSEGVs
despite the `--env` flag being passed correctly — genuinely ADVISORY-001 §3N on selkies' own fork.
`webtop_stack.sh` now also exports the tunables directly for every child selkies forks; **not yet
re-verified crash-free over many cycles** (the ACK-stall-kill below dominates the symptom in practice).

**2026-09-16: `GLIBC_TUNABLES` propagation through `spawn_exec_collision_child` has NO gap — live-proven
via a new permanent diagnostic (`lib.rs`'s `glibc_tunables_forwarded` warn line) — and the recurring
crash class is a genuinely SECOND, different corruption signature under heavy fork load (`double free or
corruption (out)` → SIGABRT, not §3N's original `REVEAL_PTR` XOR SIGSEGV), not a propagation regression
from `42d8ced`.** Confirmed forwarded `true` on every collision including the critical
`path=/lsiopy/bin/python3` selkies case, then watched the crash happen anyway ~2 minutes later on the
SELKIES_SUPERVISOR subshell. Conclusion: the tunables do their documented job and reach every process
correctly; the new signature hits the unsorted/small/large-bin paths they deliberately leave enabled,
meaning litebox's own pointer-relocation fork-healing doesn't reliably heal every plain `fd`/`bk` pointer
either under this much concurrent fork pressure. **This is Track B territory (`ADVISORY-002-d-zero-fork.md`),
not a tunable-coverage gap** — do not re-attempt a `GLIBC_TUNABLES`/env fix here without new evidence of a
THIRD mechanism. Full evidence: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

### The ACK-stall-kill — root cause still unidentified after ten investigations, the one genuinely open bug in this project

**Symptom**: streams fine, then `sk.log`'s `Client stall for 'primary'... Forcing backpressure` →
`Data WS closed ...: sent 1011 ... keepalive ping timeout` — selkies' own stall-detector kills the data
channel, and the dashboard's frontend auto-reloads. A distinct second way to land there: a fresh tab's
first connection sometimes 404s on `/websockets`, tripping the same auto-reload.

**Eight candidates investigated; seven refuted by live measurement or architecture read** (client JS/
transport, nginx config/frontend dual-connect, `/proc/<pid>/cmdline` ENOENT cost, selkies' psutil tick,
`GPUtil.getGPUs()`, pixelflux capture/encode, litebox's own NAT/`--publish` gateway). **One, fork_verify
thread-based healing starving selkies' event loop, is NOT confirmed, NOT cleanly refuted** — a real
livelock-protection gap in `on_single_step` case (1) WAS found and fixed (`b6ddf43`), stress-tested clean
11+ minutes with heals firing continuously, but no unfixed-vs-fixed A/B was possible and a disconnect has
never once co-occurred with active fork-heal traffic in ten sessions. **Do not re-reach for GLIBC_TUNABLES
here; do not re-open the frontend/nginx angle** (both byte/log-verified clean). Per-candidate evidence:
`docs/AGENTS_ARCHIVE_2026-09-15.md`.

**New, unconfirmed lead**: a live disconnect coincided with an open Thunar window closing, suggesting an
xfwm4/xfdesktop re-layout event might trigger one of `selkies.py`'s untested subprocess spawns
(`resize_display`/xrandr/xfconf-query) — untested, not ruled out.

**A follow-up needs**: a real host-TCP packet capture (Wireshark/pktmon on `127.0.0.1:3000`) correlated
against a guest-side timing instrument on selkies' `websockets`-library pong-receive path, plus a working
trusted-input path into the canvas and >1.8-2GB free memory for a rebuild-and-restart window.

**Separate open complaint, distinct from the ACK-stall-kill: Terminal Emulator/Applications-menu popup.**
Architecture read found no litebox grab-/menu-specific code on this path; driving the guest DIRECTLY
(bypassing selkies/browser) proved **both `xfce4-terminal` and the `xfce4-popup-applicationsmenu` popup
mechanism are independently healthy** (open correctly twice each, no crash) — ruling out an app-exec-crash
cause and click/render. `xdotool` on the OPEN menu (arrow-keys/type-ahead) didn't
visibly launch anything once, inconclusive (test gap vs. real defect not yet distinguished) — retry
`.wfgy/webtop_stack_menudiag3.sh`'s arrow-key variant on a quieter boot. The live browser click-path retest
remains blocked — every session this week lost its clean-enough boot to ADVISORY-001 §3N (or, 2026-09-16,
host RAM exhaustion, below) first. `net.rs` is fully cleared (live-proven twice, both guest-internal and
`-p`-external probes); the masked-502/404 bug was a startup race (fixed: `SELKIES_PORT_UP` gate + a missing
`50x.html`) plus §3N hitting selkies moments after bind (still open). `spawn_exec_collision_child`'s own
unbounded-wait hang is fixed and live-reconfirmed (`42d8ced`, 20s/120s bounded). No litebox source change
was made or warranted here — both paths are proven healthy; forcing a change without a further-specific
defect would violate this project's standing discipline. Full blow-by-blow: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

**2026-09-16, Track A fork-without-exec audit (ADVISORY-002 §6): `.wfgy/webtop_stack.sh`'s own supervisor
subshells were themselves a live-matching instance of the crash class — fixed, but crash-frequency
evidence is inconclusive (host RAM exhaustion), not yet statistically confident.** dbus-daemon (`--nofork`)
and nginx (`daemon off; master_process off;`) already avoid self-daemonizing (pre-existing). But the
script's own nginx/selkies
supervisor loops ran as bare `( ... ) &` subshells — fork() with no exec(), the same unsafe shape as
`dbus-daemon --fork` — and the archive's own `spawn_exec_collision_child` investigation already
live-caught exactly this `SELKIES_SUPERVISOR` subshell SIGABRT-ing on `double free or corruption (out)`.
**Fixed**: both loops extracted to files launched via `/bin/sh file &` (real fork+exec, discards any
inherited corrupted heap per ADVISORY-002 §1.5). Six boots this session (1 control, 5 fixed): every one
not killed early reached at least `DE_UP`/`DE_FALLBACK_LAUNCHED` cleanly, **zero occurrences of the target
tcache/double-free crash in either arm** — but every boot (both arms) had to be killed for RAM safety at
`DE_UP`/`SELKIES_LAUNCHED_LAST`, before the archive's own examples of that crash need several more
`HOLD`-minutes of fork pressure to appear. No regression across 5 fixed attempts; real crash-frequency
effect needs a re-run with more free RAM. Browser Terminal Emulator/menu retest not reached, same reason.
`xfsettingsd`/Thunar's own fork behavior not independently re-verified — unchanged from ADVISORY-002.
Boot-by-boot log: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

## Host-side crash machinery

**A fatal host fault dumps before it dies, ungated**: stack walk, `RECENT_FAULTS` ring
(`litebox_platform_windows_userland/src/lib.rs:789`), `RECOVERY_LOG` print with no env var needed
(`lib.rs:1950-1956`); a real OS minidump (`199655b`) comes only from the repeated-identical-fault circuit
breaker, >64 faults at one rip.

**Two dump fields mislead on an old reading**: `error_code` is synthesized, and `19740a3` made its
Present bit REAL rather than hardcoded 0 (every pre-2026-09-10 "not-present" inference is unsound);
`is_in_guest` became tri-state in `c0c1472`. Exact semantics: archive.

**An unexplained `0xC0000005` may be a panic** — litebox no longer treats Rust panics as panics; the
handler now enters only for the four codes it triages (`78dda05`), registers FIRST in the chain
(`5870ab0`), sizes per-depth frames from disassembly not guesswork (`5cacf7f`, `0473cc3`), and `dc108fb`
stopped the watchdog killing a recovered run. Narrative: `docs/veh-exception-handler-design.md`.

**Cross-process sync on Windows is a hard platform constraint** (memory `mem-b709a7d784b98110-1430`):
every native address/TID-based wait is process-local (`WaitOnAddress`, keyed events,
`NtAlertThreadByThreadId`=ACCESS_DENIED); only a shared kernel object crosses processes — NAMED
auto-reset Events, no `DuplicateHandle` needed. Live-verified primitive:
`litebox_platform_windows_userland/src/xproc_sync.rs` (`b2166c4`); wiring it into `RawMutex` is open (PRD
`wire-xproc-sync-crossprocessmutex-into-litebox-platform-rawmutex`).

## Closed — do not re-attempt without a genuinely new approach

**There is no open host crash** — the `RtlpUnwindPrologue` crash earlier notes called "the one genuinely
open" one was `VEH_FRAME_STRIDE`, closed by `0473cc3` (`271cbb5`): a 4096-byte per-level slice 168 bytes
short of the two frames it must cover, nested to `veh_depth=2` by `fork_verify`'s own AV-heal storm.
Bisected live: `66bd640`/`b0fe210` 10/10 fatal, `0473cc3` 0/10, `5683a4e` 0/57 (mechanism: memory
`mem-c62454fedb1baef8-2714`). Unguarded, not a live defect: PRD `veh-frame-stride-has-no-overflow-guard`.

**Windows CoW-mmap performance**: zero practical effect on tar-packed execs (`MapViewOfFile3` needs 64KiB
file-offset alignment; ELF `PT_LOAD` segments are only page-aligned, no exploitable slack).
**`LITEBOX_COW_MMAP` default-off is load-bearing** — the shipped flank fix (`329174b`) recommits orphaned
flanks as zero-fill, while the bug it fixed was `ld.so` reading that flank's `.gnu.hash`/`.dynsym`;
opting in trades a loud SIGSEGV for silently zeroed symbol tables (`docs/cow-mmap-fixed-address-design.md`,
memory `mem-3e13872ce1ffe95e-2814`).

**Input latency**: three real bugs fixed and verified live (sub-pixel remainders now accumulated
losslessly; two evdev reports per move now one `SYN_REPORT`, `59e4ca0`/`5683a4e`; window now resizable
with scaled deltas). Present mode is Mailbox-preferred with Fifo fallback (`presentation.rs:1064-1080`) —
any note calling it Fifo-only is stale. Open: PRD `mouse-motion-devicevent-needs-pixel-calibration`,
`linux-macos-userland-presentation-still-emits-two-syn-reports-per-move`; no framerate baseline exists
since an idle compositor legitimately produces zero page flips.

**The GUI protocol decision is settled**: DRM/KMS + wgpu, proven live with guest page-flip pixels in a
real host window (memory `mem-3c4a9980a884604b-1031`). Not an open X11-vs-Wayland-vs-DRM question.

## Docs and tooling map

- **Archives** — `docs/AGENTS_ARCHIVE_2026-09-16.md` (popup-menu re-test, `spawn_exec_collision_child`
  fix, Track A audit boot logs), `docs/AGENTS_ARCHIVE_2026-09-15.md` (ACK-stall-kill detail), and
  `docs/AGENTS_ARCHIVE_2026-09-10.md` (fork fd eligibility, cost history, OCI cache, s6-boot, browser
  config, crash-dump/VEH, CoW, working practices). Older: `_2026-09-03.md`, `_2026-09-05.md`.
- Fork: `docs/track-b-fork-fix-progress.md`, `advisor/ADVISORY-002-d-zero-fork.md`,
  `advisor/ADVISORY-001-fundamentals.md` (§3N tcache analysis, Appendix D presenter case).
- `docs/veh-exception-handler-design.md` — canonical VEH narrative; read before touching the handler,
  trampoline or frame sizing.
- Desktop logs: `docs/webtop-debian-selkies-2026-09-06.md`, `docs/webtop-alpine-mate-2026-09-07.md` (its
  2026-09-10 addendum has the `s6-rc.d` chain), `docs/webtop-debian-xfce-2026-09-08.md`,
  `docs/webtop-xfce-code-vs-data-2026-09-08.md`, `docs/fork-fs-veh-2026-09-08.md`.
- Consult before deriving: `docs/premade-library-research.md`, `docs/drm-dumb-buffer-ioctl-reference.md`
  (kernel UAPI for `litebox_shim_linux/src/syscalls/drm.rs`), `docs/diag-timeline-field-semantics.md`
  (before any hypothesis on `DIAG_TIMELINE`'s `comm` field — two investigations mis-traced it).
- `docs/macos.md` — port state. `litebox_platform_macos_userland::guest::run_thread` is a stub needing
  real Apple Silicon with codesign/JIT-entitlement tooling, so it stays deferred (PRD
  `macos-aarch64-guest-execution-context-switch-is-not-implemented`,
  `gui-macos-presentation-runner-and-guest-entry-blocked`).
- Probe crates: `docs/wayland-drm-backend-probe/` (musl-cross Smithay compositor driving litebox's
  virtual connector; surfaced `DRM_IOCTL_MODE_OBJ_GETPROPERTIES`/`GETPROPERTY`, a debug-build `--gui`
  stack overflow, nested epoll — `1e1da7c`); `docs/linux-native-drm-gui-probe/` (Linux-native DRM → wgpu
  control case for a Windows-only claim).
- Designs not implemented: `docs/presenter-process-design.md`, `docs/session-daemon-design.md`
  (`litebox_termemu`, its VT100-emulator slice, IS implemented; the daemon/IPC layer is not),
  `docs/fork-region-grouping-design.md` (shipped state is still a diagnostic probe).
- `advisor/probes/` — diagnostics (`decode_frame.py`, `symbolize_litebox_crash.py`,
  `cross_process_fork_wait_hang_probe.sh`, `drm_flip_probe.c`, `clone_probe.c`, `run_xfce_xwm.sh`) plus
  `MEASUREMENT-PITFALLS.md`, `DISK-HYGIENE.md`, `fork_verify_third_exec_repro.md`. OCI-pull Python
  scripts there are retired.
- `.gm/memories/`: `mem-c62454fedb1baef8-2714` (RtlpUnwindPrologue), `mem-e5107049137fcf43-1303`
  (2026-09-15 browser witness), `mem-7cb09e839ca086f2-4223` (XFCE/MATE/weston), `mem-6c4697ac568ea7be-4487`
  (packager OOM), `mem-136ae2ce29bc28a4-3133` (image tags), `mem-b709a7d784b98110-1430` (cross-process
  sync), `mem-f17269d5777055d3-3326` (2026-09-07 defects), `mem-3e13872ce1ffe95e-2814` (CoW),
  `mem-3c4a9980a884604b-1031` (GUI protocol).
