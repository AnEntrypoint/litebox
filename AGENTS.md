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
  path.
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
  host; kill every `litebox_runner` between runs.
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
- **Repo hygiene** — packed layer tars, frame dumps and debug logs never go in git; keep them in `.wfgy/`
  (gitignored) or an untracked sibling like `../litebox-webtop/`. Root-level scratch (`probe_*.tar`,
  `*.bmp`, `*.log`) is gitignored; if `git add -A` sweeps one in, untrack it.

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

**Which X server**: for the DRM/wgpu on-screen (`--gui`) path use `Xorg` with `modesetting` — litebox's
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

### The ACK-stall-kill — root cause still unidentified after ten investigations, the one genuinely open bug in this project

**Symptom**: streams fine, then `sk.log`'s `Client stall for 'primary'... Forcing backpressure` →
`Data WS closed ...: sent 1011 ... keepalive ping timeout` — selkies' own stall-detector kills the data
channel, and the dashboard's frontend auto-reloads, presenting as "needs a refresh." A distinct second
way to land there: a fresh tab's first connection sometimes 404s on `/websockets`, tripping the same
auto-reload.

**Eight candidates investigated; seven refuted by live measurement or architecture read, one still open**
(full evidence for each: `docs/AGENTS_ARCHIVE_2026-09-15.md`):

- Client JS/transport — refuted (protocol-only WS client survived 150s clean; real browsers died on the
  same endpoint at the same time).
- nginx config / frontend "dual-connect" — refuted, byte- and log-verified; the "two `Legacy client`
  registrations per reload" is selkies' own by-design reconnect pairing, not a bug (it only *repeats
  forever* here because the ACK-stall-kill fires every cycle).
- `/proc/<pid>/cmdline` ENOENT cost (the original 19/19-correlated marker) — refuted, measured 5-10us/
  occurrence, 5-6 orders of magnitude below the 20s timeout window. A correlated marker, not a cause.
- selkies' own psutil tick — refuted, measured 74.8us/tick combined (pinned deployed source has no
  per-process enumeration at all).
- `GPUtil.getGPUs()` — refuted, measured 0.6-0.85ms/call (fork+failed-exec, `nvidia-smi` absent).
- pixelflux capture/encode — refuted by the library's own upstream source: dedicated delivery thread, one
  GIL acquisition per frame, nothing on the event-loop thread. Corroborated live: a disconnect fired while
  the desktop was idle, the opposite of what an encode-load cause predicts.
- litebox's own NAT/`--publish` gateway (`net.rs`) — refuted by full 1068-line read: one thread, fixed
  5ms tick, fully nonblocking both directions. (Unrelated cleanup flagged, not the cause: `LoopbackQueue`
  has no size cap and clones its whole backlog every tick.)
- **fork_verify thread-based healing starving selkies' event loop — NOT confirmed, NOT cleanly refuted.**
  No priority/affinity/global-suspend mechanism found anywhere in the crate. A real litebox-only defect
  WAS found and fixed here: `on_single_step` case (1) had no livelock protection (unlike its AV-path
  siblings), so an unhealed repeat address cost a fresh ~600us trap every iteration (measured: 357
  consecutive traps, 216ms). **Fixed `b6ddf43`**: case (1) now gets the same livelock counter and deeper
  GOT/PLT healers the AV path already has. Stress-tested same day: 0.3s-interval concurrent fork load for
  11+ minutes of disconnect-free streaming with the fix active and heals firing continuously (124k+ warn
  lines) — real evidence the fixed defect isn't THE cause *by itself*, but no unfixed-vs-fixed A/B was
  possible (single-runner host). Across all ten sessions, a disconnect and active fork-heal traffic have
  never once co-occurred in the same observation window, either direction.

**Do not re-reach for GLIBC_TUNABLES here** (zero fatal-signal lines near any kill/stall/reload). **Do
not re-open the frontend/nginx angle** (byte/log-verified clean). **Do not re-attempt the thread-priority/
global-lock/suspension angles** under the fork_verify candidate (checked clean above).

**New, unconfirmed lead**: a live disconnect coincided with an open Thunar window closing, suggesting an
xfwm4/xfdesktop re-layout event might trigger one of `selkies.py`'s untested subprocess spawns
(`resize_display`/xrandr/xfconf-query). Synthetic pointer events via chrome-devtools don't register with
the guest's real input path, so this is untested, not ruled out.

**What a follow-up needs that none of the ten had**: a real host-TCP packet capture (Wireshark/pktmon on
`127.0.0.1:3000`) correlated against a guest-side timing instrument on selkies' `websockets`-library
pong-receive path — direct wire-vs-guest timing, not one more candidate ruled out by absence. Needs a
working trusted-input path into the canvas (`claude-in-chrome`'s coordinate `computer` tool, untested
across multiple sessions) to drive load on demand, and a host with >1.8-2GB free memory for a
rebuild-and-restart window.

**Separate open complaint, distinct from the ACK-stall-kill**: Terminal Emulator reportedly never opens
in the streamed desktop. Not a general hang — native coordinate-precise clicks open Thunar in seconds
even under continuous fork-stress load — but every popup/dropdown menu (XFCE panel "Applications", Thunar
"File") fails to open via the same clicks that work everywhere else, not yet root-caused (X11
pointer-grab semantics for `GtkMenu` vs. a menu-specific timing/coordinate issue). A session with working
popup-menu input, or a guest-side `xdotool` path, should open Terminal Emulator directly and time it.

**2026-09-16**: a live re-test attempt (blocked before reaching the Applications menu; architecture read
found no litebox grab-/menu-specific code on this input path) hit selkies never binding
(`/websockets` 404) — now root-caused and **fixed**: `spawn_exec_collision_child`
(`litebox/src/platform/mod.rs:1131`, impl `litebox_platform_windows_userland/src/lib.rs`) recovers from
an exec-address collision by spawning a replacement `litebox_runner`, but awaited it with an unbounded
blocking `cmd.status()` — a wedged replacement hung the WHOLE guest boot forever. **Fixed and
live-verified (`42d8ced`)**: bounded poll (20s stall grace/120s cap); re-run hit the same collision, the
cap fired, and supervisor-respawn recovered cleanly. Does not fix Track B or ADVISORY-001 §3N (above,
both still open) — only bounds this hang. Full repro/architecture-read detail:
`docs/AGENTS_ARCHIVE_2026-09-16.md`.

**2026-09-16, later same day: `42d8ced` re-confirmed live twice more; Terminal Emulator app and the
Applications-menu popup both proven healthy in isolation; the live menu-click test itself is still
blocked by the pre-existing `-p` NAT websocket bug.** 13 boot attempts of `.wfgy/webtop_stack.sh
--resume-from .wfgy/webtop_stack_seed.tar` this session. Two live firings of the `42d8ced` bound, both
the same `/lsiopy/bin/python3` collision, both hit the 120s cap and recovered without hanging the guest:
once the guest kept serving the dashboard throughout (curl `200` on `/`); once the fallback SIGSEGV
happened to land on the selkies-supervisor subshell itself (`comm=sh`, not the colliding child), which
permanently lost selkies for that boot (no more respawns) but still did **not** hang the whole
guest — a related but distinct failure mode from the originally-fixed hang, worth a future look but not
a defect in the fix itself.

Could not reach a live, browser-rendered desktop to click-test the Applications menu that session: every
external (`-p`-published) request to `/websockets` returned `404` (masked `502`) deterministically, even
on a clean boot with selkies confirmed listening — the same bug `docs/webtop-debian-selkies-2026-09-06.md`
first captured. A same-day interface-address workaround (proxy_pass to `10.0.0.2:8081` instead of
`127.0.0.1:8081`) reproduced the `404` identically, ruling out loopback-vs-interface-address as the
variable.

**Root-caused live later the same day — `net.rs` cleared for good, this thread closed.** Tailed nginx's own
`error_log` live for the first time and caught the real error on two boots: a genuine
`connect() failed (111: Connection refused)` to `127.0.0.1:8081`, from BOTH a guest-internal probe
(`client: 10.0.0.2`) and a real `-p` external probe (`client: 10.0.0.1`) — proving live what `net.rs`'s
code already implied (its loopback fast-path bypasses the NAT gateway entirely for `127.0.0.0/8`), so this
was never a `-p`-vs-loopback bug. The refusal is (a) a plain startup race (selkies' real bind latency is
~100-140s; anything proxied earlier gets a genuine ECONNREFUSED) and/or (b) the standing ADVISORY-001 §3N
tcache class killing selkies moments after it DOES bind (live-witnessed: bound, logged "listening on port
8081", then still 502'd on a probe shortly after — and this session's own crash dump caught a NEW instance
of this class hitting the boot script's own `pid=2`, not just selkies/nginx). Two `.wfgy/webtop_stack.sh`
fixes landed (gitignored, no litebox source change): a missing `50x.html` meant every real 502 was itself
404ing — fixed, so a genuine backend failure now surfaces honestly; and a `SELKIES_PORT_UP` gate (curl exit
code, not `%{http_code}`, which can't tell "nobody home" from "connected, no HTTP reply" against a raw WS
server) closes the first-bind race before anything downstream treats the boot as ready. (b) remains open,
unchanged, under ADVISORY-001 §3N. Evidence trail: `docs/AGENTS_ARCHIVE_2026-09-16.md`. Browser Terminal
Emulator re-test still not reached — both boots run to gather this evidence were themselves lost to §3N
before a long-enough clean window opened.

With the browser path blocked, tested the menu bug a different way: drove the guest DIRECTLY (no
selkies/browser/click in the loop) via a script step after `DE_UP`. Two clean findings, each reproduced
twice across independent boots:

- **`xfce4-terminal` itself is completely healthy under litebox.** `DISPLAY=:1 xfce4-terminal
  --title=DIAGTERM -e ...` opens a correctly-sized real window (`818x485`) within 3 seconds every time,
  no crash, only a benign `SESSION_MANAGER` warning. Rules OUT an app-level exec/fork crash (hypothesis
  (c)/(d) from the prior investigation) as the cause of "Terminal Emulator never opens."
- **The Applications-menu popup mechanism itself also works.** `xfce4-popup-applicationsmenu` (the exact
  helper the panel button execs) reliably creates a real `166x305` menu window both times tried — input
  reaching the button and the popup rendering are NOT the broken link.
- **Inconclusive, not yet resolved**: driving the OPEN menu with `xdotool` (type-ahead search "Terminal
  Emulator" + Return, once; arrow-key Down/Right exploration, attempted twice more) did not visibly
  launch anything in the one run that completed the step, but both later arrow-key attempts were
  preempted by this session's own elevated crash rate before completing (one full guest death, `pid=2
  comm=sh SIGSEGV`, mid-boot before reaching the diagnostic; one run where the runner's own RSS grew past
  5GB with stalled stdout progress, killed rather than risk host OOM). Whether the open menu genuinely
  fails to dispatch activation, or the test itself has a gap (type-ahead search may not traverse a nested
  category from the root level), is NOT yet distinguished. Next session: retry the arrow-key-only variant
  (script already written, `.wfgy/webtop_stack_menudiag3.sh`) on a quieter boot — if keyboard nav also
  produces nothing, that is strong evidence of a real dispatch-level defect in this popup instance (a
  structurally different code path from mouse clicks, further narrowing away from mouse-only
  grab-semantics theories if it reproduces).

This session's boot reliability was noticeably worse than the 3/5 baseline the nginx-race fix
established: 3 of 13 attempts hit `XVFB_FAILED`, at least 2 hit a full-guest `pid=2 SIGSEGV` (the
standing ADVISORY-001 §3N tcache class, not a new defect), and one run's RSS grew to 5GB+ with no
forward stdout progress before being killed — not root-caused, plausibly the same tcache class
manifesting as a slow spin rather than an immediate abort, plausibly amplified by this session's own
extra forking (`xfce4-terminal`, `xfce4-popup-applicationsmenu`, `xdotool`) adding more exec/collision
surface on top of the normal boot sequence. Flagging, not chasing further this session.

No litebox source change was made or warranted for the Terminal Emulator/menu investigation this
session — both the app-exec path and the menu-popup path are now independently proven healthy, and
forcing a change without isolating a further-specific defect would violate this project's own standing
discipline against unverified fixes.

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

- **Archives** — `docs/AGENTS_ARCHIVE_2026-09-16.md` (popup-menu re-test,
  `spawn_exec_collision_child` fix detail), `docs/AGENTS_ARCHIVE_2026-09-15.md` (30KB recompile + the
  ACK-stall-kill detail), and `docs/AGENTS_ARCHIVE_2026-09-10.md` (fork fd boundary/eligibility, per-fork
  cost history, 2026-09-08 defect set, OCI cache internals, s6-boot gaps, browser-desktop config,
  crash-dump/VEH constants, CoW flank fix, working practices). Older: `docs/AGENTS_ARCHIVE_2026-09-03.md`,
  `_2026-09-05.md`.
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
