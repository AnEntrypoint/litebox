# litebox — current state (2026-09-10)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every
claim carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a
claim nobody could point at was deleted rather than hedged. Reference detail is drained to
`docs/AGENTS_ARCHIVE_2026-09-10.md`, older narrative to `docs/AGENTS_ARCHIVE_2026-09-03.md` and
`docs/AGENTS_ARCHIVE_2026-09-05.md`, per-investigation logs to the dated `docs/*.md` in the map below
— read those for a trail, never as a starting point.

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
default was discarding all 111 real `warn!` sites; `fork_verify` is pinned to `error` because it warns
per single-stepped instruction). Do **not** add `LITEBOX_LOG=error` by reflex — it suppresses exactly
what the default exists to surface. Use `fork_verify=warn` when a fork heal is the subject.

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
  hang. Each runner under an OCI desktop image holds 650MB-1GB+ resident and this host has been seen
  with ~800MB free; kill every `litebox_runner` between runs.
- **`LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check** (numbered `.bmp` +
  non-black-pixel count to stderr), never `PrintWindow`/`CopyFromScreen`.
- **A pixel count never identifies WHO painted a frame** — decode frame structure
  (`advisor/probes/decode_frame.py`) and correlate against `DIAG_TIMELINE execve`'s real argv0. Cost real
  time twice: weston's own panel mistaken for XFCE's, and panel-shaped pixels attributed to
  `xfce4-panel` on an image that ships MATE and has no `xfce4-panel` at all.
- **Never time litebox with one host process per datapoint** — a bare spawn costs 1.6-2.3s, dwarfing real
  per-exec differences. Run N iterations inside ONE guest process and take the delta, hold host load
  constant, and establish a noise floor (10+ runs): this host shows a ~20-36% spread that retracted
  several single-shot "findings". And never subtract timestamps across a parent log and a fork-child log
  — every child is a fresh re-exec whose `init_logging()` resets elapsed time to ~0 (`4b95600`).
- **Refusal errno choice is API contract.** EPERM lets callers degrade; EINVAL/ENOSYS makes them fail
  hard. A `clone()` namespace-flag EINVAL once silently broke ALL PNG/JPEG decode via glycin's bwrap
  fallback, and an unknown-socket-option EINVAL broke GdkPixbuf→glycin→D-Bus decode for every format
  (`694bb93`, now `ENOPROTOOPT`). `EOPNOTSUPP` is itself fatal on glibc (`futex_fatal_error()`) — that
  killed selkies one line after the cursor was delivered (`a120c56`).
- **Proving a run took the cross-process fork path needs `[process_fork_diag] task-resume-probe` lines,
  never the shim's eligibility log.** `LITEBOX_PROCESS_FORK` is read at the first statement of
  `spawn_cross_process_fork_child` (`litebox_platform_windows_userland/src/lib.rs:9758`), while the shim
  logs `clone: cross-process fork() is eligible` (`litebox_shim_linux/src/syscalls/process.rs:2807`)
  regardless. This trap produced two recorded false conclusions (archive).
- **fork carries pipes, regular files and the writable layer into a child, but NOT sockets** — so
  `dbus-daemon --fork` serves nothing: the socket is created pre-fork, the child accepts on nothing, the
  client's `connect()` still succeeds against the filesystem path and hangs in `ppoll(timeout=-1)`
  awaiting a SASL reply. Run dbus-daemon non-forking — this one fact was the entire MATE black screen
  (`181ec68`). For XFCE use `xfce4-session`, never `startxfce4` (that starts a second bus on a different
  address).
- **Use `advisor/probes/symbolize_litebox_crash.py` on host-side crashes, and snapshot `.exe` + `.pdb`
  next to the log.** A ring dump's `rva=` is meaningful only against the exact emitting build;
  symbolizing against a rebuild gives confident, plausible, wrong names. Only `is_in_guest=false` entries
  carry a real module address.
- **Isolate the harness before blaming litebox** — launch guest probes directly as the runner's top-level
  program from their own minimal tar, never via a runtime-built `/bin/sh -c` wrapper. MSYS2 path mangling
  and a shell SIGILL each produced a false "litebox is fundamentally broken" claim that vanished once the
  harness variable was removed.
- **Never trust a container tag name for its WM/session contents** — verify by registry manifest + blob
  tar-listing, or a live in-guest `/usr/bin` listing. Earned three times.
- Procedural know-how lives in the archive's "Working practices": building freestanding guest binaries on
  the HOST (both guest compilers are broken), injecting a probe via a small `--resume-from` overlay tar
  and the two env vars that needs, building observability proactively, and preferring mature libraries
  over hand-rolled code for known problem classes.
- **Never record a test count you did not just watch run to completion, and never leave a suite red for
  an environmental reason.** (Supersedes any older "26 failing, 9 need `diod`" note. No counts are
  recorded here on purpose — a suite is not evidence of anything.)
- **Repo hygiene** — packed layer tars, frame dumps and debug logs never go in git; keep them in `.wfgy/`
  (gitignored) or an untracked sibling like `../litebox-webtop/`. Root-level scratch (`probe_*.tar`,
  `*.bmp`, `*.log`) is gitignored; if `git add -A` sweeps one in, untrack it.

## Guest-reachable code returns an errno, never a panic

The host process IS the entire guest session, so an `unimplemented!()`/`unreachable!()`/panic, or an
unbounded recursion, on any guest-reachable path kills every guest process at once. Bitten at least
three times; the fix is always the same shape — report what is true, as an errno.

- **`6e14b71`** — `litebox/src/fs/layered.rs`'s `chmod`/`chown`/`set_times` mapped `migrate_file_up`'s
  `MigrationError::NotAFile` to `unimplemented!()`. `migrate_file_up` migrates byte *contents*, so every
  lower-only **directory** or character device took that arm: `touch /usr/share` and `chmod` on any
  read-only-layer directory were deterministic HOST panics at `layered.rs:1538`, as was `touch
  /dev/null`. All three also ended in an unbounded `self.<op>(path, …)` tail call that overflowed the
  host thread stack whenever the upper layer still reported the path missing. Fixed by
  `migrate_entry_up_for_metadata` (recreates a lower-only directory in the upper layer with the lower's
  own mode, carrying `node_info` so the inode is stable across the copy-up — measured 17 → 17 → 17),
  `EROFS` for kinds that cannot be carried up (chardev, FIFO), and a bounded two-pass loop in the three
  callers.
- **`133d3d4`** — `O_NOATIME` panicked the host; `litebox/src/fs/in_mem.rs:282` is the same class.
- Same class, all fixed: `62e3c79`/`44189c7` (OOM panicked instead of `ENOMEM`), `ab2393d` (`listen()`
  panicked on `backlog=0` or re-listen, both of which real nginx does), `a473048` (a guest open flag could
  kill the host), `8aa05af` (a corrupted guest context kills that guest, not the host), `5ec1ee4` (a
  debug-only diagnostic crashed the host in the exact scenario it existed to debug), `1e1da7c` (nested
  `epoll_ctl(EPOLL_CTL_ADD)` hit `unimplemented!()`; calloop does exactly this).

## Cross-process fork (`LITEBOX_PROCESS_FORK=1`)

A genuine `D == 0` fork — child at the SAME addresses, no relocation, no `fork_verify` healing — exists as
`spawn_cross_process_fork_child` (design case `advisor/ADVISORY-002-d-zero-fork.md`).
`try_cross_process_fork` (`litebox_shim_linux/src/syscalls/process.rs:2524-2814`) short-circuits to a
native fork when `platform.has_native_fork()`, so the whole fd-carrying apparatus is Windows-only
scaffolding for a missing syscall.

**It is correctness-sound** (`71a4bcd`): a minimal repro (`bash -c` doing `x=$(echo hi)` in a loop),
confirmed on-path via `task-resume-probe`, shows zero corruption across every completed fork, against the
thread-based default's 100% `malloc(): unaligned tcache chunk detected` rate on the identical repro.
ADVISORY-001 section 3N's tcache corruption is the **thread-based** path's defect only. That conclusion
was reached, retracted and re-reached (chain in the archive), and the `fd_complexity.beyond_stdio == 0`
check older notes call the gate is not one — `b2c89fc` found that assert emitting something false 67
times a boot.

**Eligibility** — refused only for `comm` == `Xvfb` or `dbus-daemon` (`process.rs:2560`, `564af3f` — a
live unix listening socket cannot be served from a fork-time filesystem snapshot), an already-borrowed
fd table, a beyond-stdio fd that is not a pipe end / path-recorded regular file / eventfd / close-on-exec
(overridable by `LITEBOX_PROCESS_FORK_IGNORE_FDS`), or an unsanitizable `fs_base`/context. On a real
`debian-xfce` boot the only remaining blocking kind is `unix-socket` — 5 refused forks of 34, down from
34/34 (`ed74c28`). Exact line numbers, and the per-kind deviations that matter (file offsets and eventfd
counters are **copied, not shared**; CLOEXEC fds are dropped, so a child using one before `execve` gets
`EBADF`): archive. Read it before assuming a guest gets real `fork()` sharing semantics.

**Per-fork cost** was ~3.5-5s, now ~1.2s (`1199ab6`, `cc7986f`, `ce5648f`), and a full `webtop_stack.sh`
boot reaches `NGINX_CONFIGURED`/`NGINX_STARTED` in under a minute versus never in 15+. The
rootfs-re-merge and `WindowsUserland::new()` explanations older notes give are **measured wrong** and
ruled out by name (`78040e3`), as is writable-layer growth (`6d4248b`/`c208e12`). Use
`LITEBOX_DIAG_FORK_TIMING=1` for the next cost question; measurement history: archive.

**Three correctness bugs the perf work exposed, all fixed** — `060ccc3` (no `SIGCHLD` reached a parent
from a cross-process child, hanging any parent that used the race-free mask-then-`sigsuspend` wait),
`6e86a40` (`sys_wait4(pid=-1)` checked the cross-process registry only when the thread-based one was
already empty), `d5cc744` (a redundant claim release deleted a coalesced `CLAIMED_RANGES` slot and with
it collision coverage for a loaded library). Mechanisms and repros: archive.

**Reading a cross-process log**: on an identity fork, `fork_verify` emits "stale CODE pointer detected,
translating and resuming" with `translated_rip == rip` — 84,319 of ~90,400 lines in one run, zero real
translations, bounded per child by `MAX_IDENTITY_VERIFICATION_STEPS = 4096`. Wasteful, not corrupting;
without knowing this you will chase it.

**Still open**: nginx's own SSL-cert generation fails on its first real startup attempt — the original
symptom this investigation began from, genuinely not root-caused
(`docs/track-b-fork-fix-progress.md:146-152`). Do not cite `1f30ab4` as live open work: that was the
separate curl-self-test stall, fixed by `6e86a40`.

## Container images and OCI loading

**`litebox_packager --oci-image <ref> --output <tar>`** pulls, whiteout-merges, rewrites every ELF and
produces a bootable flat tar in one command (`litebox_packager/src/lib.rs:43-51`; x86-64 and Apple
Silicon hosts only, `:114`). It supersedes every ad-hoc Python script this project once hand-rolled for
the job (`pull_oci_image.py`, `batch_rewrite_layer.py`, `fetch_container.py` — retired, do not recreate).

**Runtime in-memory loading** — `--oci-image <ref>` pulls, merges and rewrites every layer in memory; no
host directory is ever created for the rootfs (extracting to a real one was the original approach and hit
three independent Windows-path bugs). `litebox_runner_linux_on_windows_userland/src/lib.rs:94` (mutually
exclusive with `--initial-files`), `:630-638`, `:1350` (fork-child re-derivation). Rewritten layers are
cached under `.litebox-cache/` keyed on `(layer digest, REWRITER_CACHE_VERSION)`, so a rewriter change
self-invalidates. Large images pack fine now (`alpine-mate` 2937.9MB/59,067 entries; `ubuntu-xfce`
119,692 entries/13.7GB); residual risk is host-memory contention from unrelated processes, not a litebox
bug. `tar_ro.rs`'s multi-layer index is built ONCE at mount (`litebox/src/fs/tar_ro.rs:61,75-80`), not
per read — that build was O(entries²) and starting one process against the 2.5GB webtop rootfs went
17.3s → 0.35s (`90c010a`). Cache internals and the four fixed OOM bugs: archive.

**Trampoline-extension failure used to poison a whole segment's syscalls, not just the
overflowing ones** (`6311f747b3`). The runtime patcher's initial trampoline allocation
was a flat one-page guess; a segment needing more stub space (a few hundred `syscall` sites —
ordinary for a real binary, not just busybox) then tried to *extend* the region at exactly one
fixed adjacent address (`MAP_FIXED_NOREPLACE`, no fallback — unlike the initial allocation's own
try-fixed-then-let-the-VM-choose path a few lines up). Any unrelated mapping already occupying
that one address made the extension fail, and `apply_trap_fallback` then poisoned **every**
syscall in the whole segment with `ICEBP;HLT`, regardless of how many were otherwise patchable —
so the guest died on the first syscall it executed after load. Confirmed live pulling+booting
`docker.io/edgelevel/alpine-xfce-vnc:latest` fresh via `--oci-image` (no packager step): busybox
`/bin/sh` SIGILL within 3s of exec (`[diag-ud-entry] raw_code=0xc0000096`, 480 sites poisoned by
one failed 4KiB extension). Fixed by sizing the initial allocation from a cheap `0F 05` byte-pair
count (sound upper bound — same technique the rewriter's own fast-reject scan already relies on),
capped at 4MiB; re-run clean, zero fatal signals. Generalizes beyond this one image: any real
binary whose patchable-syscall count exceeds one page of stubs was exposed.

**`edgelevel/alpine-xfce-vnc` verified against its canonical registry layer**: Alpine 3.16.0,
ships `Xvfb`/`x11vnc`/`novnc_server` plus the full `xfce4-session`/`xfwm4` set — a noVNC-over-
browser pipeline, same X-server category (Xvfb, not Xorg/DRM) as the already-verified
`alpine-mate` selkies boot above, so there is no DRM/KMS+wgpu path for this image to misalign
with; it lands squarely in the already-settled Xvfb/browser pipeline, not the `--gui` one.

**Tags, verified live, never from the name**: `linuxserver/webtop:alpine-mate` ships MATE, not XFCE;
`alpine-xfce` does not exist (404); `debian-xfce`/`ubuntu-xfce` DO ship a real XFCE stack (`34da133`,
`c65ab93`, `1ea5203`; XFCE ships only on the debian/ubuntu/fedora/arch bases, `8c07f51`). The `alpine-*`
flavors share one ~519MB base layer (`9c7ea2b`); `debian-xfce` is a 17-layer Debian 13 image sharing
nothing with them. `ubuntu-xfce` packs fine but its rust-coreutils aborts in rustix auxv handling, taking
out `sleep`/`tail` and the DE launch — `bb46f1a` has since implemented `/proc/self/auxv` and `AT_EXECFN`,
so that is one re-test, not a fresh investigation (archive).

**Which X server**: for the DRM/wgpu on-screen (`--gui`) path use `Xorg` with `modesetting` — litebox's
virtual DRM device is legacy-KMS + dumb-buffer + XRGB8888 only, no atomic modeset, no GBM/EGL, so a
GBM-first compositor lands on its least-tested software-rendering fallback, and `Xvfb` never touches
DRM/KMS there at all (zero page-flips, indistinguishable from "the guest never drew"). For the
browser/selkies path `Xvfb` IS the correct and verified component: `alpine-mate`'s own `svc-xorg/run`
execs `/usr/bin/Xvfb` (`/usr/bin/Xorg` is a 275-byte sh wrapper no service invokes), and its `-shmem`
framebuffer works now that SysV shared memory exists (`4abf971`; it previously exited cleanly with
`shmget: ENOSYS`). Both browser-verified pipelines are Xvfb-based.

**Durable artifacts**: `C:\dev\litebox-webtop\webtop_seatd.tar` (stock MATE webtop);
`.wfgy/xfce-build/layer31_direct_fixed.tar` (the older hand-assembled weston+XFCE layer, superseded in
priority by the stock-image path).

## A real desktop renders in a browser

**The XFCE desktop renders in a real host browser** — Chrome on the host showing xfdesktop's icons, the
cursor and live H264 pixels off the guest's X server, reproduced twice on a fresh stack (`f10c5e9`). The
whole pipeline (Xvfb, X clients, selkies/pixelflux x264, MIT-SHM capture) runs inside litebox; only the
reverse proxy is host-side. **A default-configured MATE desktop also renders**, panels and menus and
input round-trip included (`8c07f51`, `181ec68`). The exact working configuration — selkies
`--addr=0.0.0.0` on port **8081**, dashboard over `--publish`, `/websockets` tunnelled to 8081 — is in
the archive, along with what was actually on screen.

**A stock s6-overlay image boots with no flags and no stubs**: `/init` on `webtop_seatd.tar` runs 16
cross-process children with zero uncarriable fds and zero fatal errors, all the way into supervision
(`66bd640`, `docs/fork-fs-veh-2026-09-08.md:35-37`). This retires three "fundamental blocker" claims
older notes carried — the s6 `/init` ET_EXEC collision, only-one-`python3`, and `failed to map segment`
under load. The filesystem gaps closed to get there: archive.

Two sets of seven independent litebox defects got the pipelines up, all landed: the 2026-09-07 set
(memory `mem-f17269d5777055d3-3326`; `4abf971` SysV shared memory was the final blocker, since pixelflux
capture needs MIT-SHM) and the 2026-09-08 set that made the *default-configured* desktop render and
fixed the missing Applications menu (`8c07f51`; enumerated in the archive).

**The black XFCE desktop had a deterministic root cause, now fixed** (`c7a1fa8`, documented `a29216b`):
the runtime rewriter patched whole `PROT_EXEC` mappings, corrupting 1984 bytes of `libLLVM.so.19.1`'s
`.dynsym`, so `dlopen` of libLLVM/libgallium/libGLX_mesa failed ~3x/second forever and `xfwm4` never
created a window. Any older note calling the black or inconsistent desktop an unexplained
non-deterministic race is superseded by this.

Settled in the archive, so nobody re-derives them: PI futexes are genuinely implemented, not refused
(`a120c56`, `a473048`); labwc's `wlr_swapchain_create` SIGABRT is an upstream wlroots legacy-DRM
limitation, not a litebox gap; every older gdk-pixbuf/GTK per-format decode finding was measured while
`694bb93` broke decode for **every** format, so re-measure before trusting any; and the two network
fixes this path needs (`537c088`, `2197d18`).

**Open here.** Selkies serves one client and a page reload does not reclaim the slot, so a fresh stack
is needed per view. An intermittent host AV ends some runs at varying points (latest shape `rip == fault
address == 0x7ff003444000`, an instruction fetch in the host-allocator region) — that, not the desktop
background, is this area's real remaining non-determinism. The one architectural gap for a full desktop
on the crash-free cross-process path: **guest processes share no AF_UNIX/loopback/FIFO namespace**, so a
cross-process fork gives zero AVs but Xvfb is unreachable from its clients (`/tmp/.X11-unix/X1` is not a
shared object); one host-side transport shared by every process of a guest would put the whole desktop
on the already-crash-free path (`docs/fork-fs-veh-2026-09-08.md:128-144`).

## Host-side crash machinery, and the one crash still open

**A fatal host fault dumps before it dies, ungated** — the stack walk, `RECENT_FAULTS` ring
(`litebox_platform_windows_userland/src/lib.rs:789`) and `RECOVERY_LOG` print with no env var set
(`lib.rs:1950-1956`); a real OS minidump (`199655b`) comes only from the repeated-identical-fault circuit
breaker, >64 faults at one rip.

**Two dump fields mislead if you trust an old reading.** `error_code` is synthesized here and `19740a3`
changed its meaning — its Present bit is now REAL rather than hardcoded 0, so **every pre-2026-09-10
"not-present, therefore unmapped" inference from `error_code` is unsound**, and a pre-fix "write" may
really have been an instruction fetch. `is_in_guest` became tri-state in `c0c1472`: before it, a thread
whose TLS identification merely failed printed identically to a confirmed `false`, and `rip`/`rsp` could
be torn across two reads of a live `CONTEXT`. Exact semantics: archive.

**An unexplained `0xC0000005` with no further detail may be something that panicked** — litebox no longer
sees Rust panics as panics. The handler used to carry every exception code, `EXCEPTION_STACK_OVERFLOW` and
the MSVC/Rust panic code `0xE06D7363` included, through a stack swap into a handler with no interest in
it, overflowing the stack; it now enters only for the four codes it triages (`78dda05`) and registers
FIRST in the chain (`5870ab0`). Per-depth frames are sized from disassembly rather than guessed
(`5cacf7f`, `0473cc3`), and `dc108fb` stopped the watchdog killing a successfully-recovered run.
Constants: archive. Narrative: `docs/veh-exception-handler-design.md`.

**The `RtlpUnwindPrologue` crash — the one genuinely open host crash.** A host-side (not guest) access
violation inside litebox's own `fork_verify` single-step healing, reached from `ntdll!RtlpUnwindPrologue`.
Repro: `mate-session --help` against `webtop_seatd.tar`, no compositor, ~30s (PRD row
`mate-session-avs-are-in-fork-verify-not-guest`; the older `--version` x3 / ~90s form also works,
slower). Surviving evidence: 66 of 66 AV events report `is_verifying=true`, and independently of any
register field the faulting stack decodes as UTF-16 `LITEBOX_DIAG_FAT…LT_VEC` — litebox's own
`GetEnvironmentVariableW` strings, which guest musl would never have on its stack. **Every
register-level claim older than `c0c1472` is void, in both directions**: neither the "`is_in_guest=true`,
`addr=usize::MAX`" first-fault claim nor the 154-event "`is_in_guest=false`, `addr=0x2` on every event"
claim raised to refute it has standing, because that ring tore `rip`/`rsp` and collapsed unknown-TLS into
`false`. There is no surviving depth-0 evidence either way. Next step: re-capture the now-fixed
`RECENT_FAULTS` ring or `diag_raw_regdump`, and **not** under `LITEBOX_VEH_TRACE=1` — tracing's overhead
dodges the race (~12 traced runs never reproduced it). Do not theorize a fix before that capture; the
standard here is a genuinely root-caused fix, not one that stops the symptom. History: memory
`mem-5ad34546c566b8d6-7306`, `advisor/probes/fork_verify_third_exec_repro.md`.

**Cross-process synchronization on Windows is a hard platform constraint** (measured, memory
`mem-b709a7d784b98110-1430`): every native address/TID-based wait is process-local — `WaitOnAddress`,
keyed events, `NtAlertThreadByThreadId` (ACCESS_DENIED). The only cross-process wake is a shared kernel
object; NAMED auto-reset Events work cross-process with no `DuplicateHandle`. The primitive exists and is
live-verified (`litebox_platform_windows_userland/src/xproc_sync.rs`, `b2166c4`); wiring it into
`RawMutex` is open (PRD row `wire-xproc-sync-crossprocessmutex-into-litebox-platform-rawmutex`).

## Closed — do not re-attempt without a genuinely new approach

**Windows CoW-mmap performance.** Zero practical effect on real tar-packed execs: `MapViewOfFile3` needs
64KiB file-offset alignment and real ELF `PT_LOAD` segments are only page-aligned with no exploitable
slack, regardless of tar-file-start alignment. **The `LITEBOX_COW_MMAP` default-off is load-bearing**:
the shipped flank restoration (`329174b`) recommits orphaned flanks as anonymous **zero-fill**, while the
fault it fixed was `ld.so` **reading** that flank's `.gnu.hash`/`.dynsym` — so opting in trades a loud
SIGSEGV for silently zeroed symbol tables. Detail: archive,
`docs/cow-mmap-fixed-address-design.md`, memory `mem-3e13872ce1ffe95e-2814`.

**Input latency.** Three real bugs fixed and verified live: sub-pixel mouse remainders truncated by
float-to-i32, now accumulated losslessly; each physical movement delivered as TWO evdev reports instead
of one, now a single `SYN_REPORT` (`59e4ca0`); the window locked to the guest's virtual resolution, now
resizable with scaled deltas. Present mode is **Mailbox-preferred with Fifo fallback**
(`presentation.rs:1064-1080`, `5a194f5`/`aa1d0ca`) — any note calling it Fifo-only is stale, as is the
whole `evdev-emits-two-syn-reports-per-mouse-move` PRD row. Genuinely open: deltas come from differencing
winit `CursorMoved` absolute positions rather than `DeviceEvent::MouseMotion`, so sub-pixel accumulation
mitigates quantization but not winit's event rate; and there is no framerate baseline, since an idle
compositor with no client legitimately produces zero page flips — a real moving on-screen client first.

**The GUI protocol decision is settled**: DRM/KMS + wgpu, proven live with guest page-flip pixels in a
real host window (memory `mem-3c4a9980a884604b-1031`). Not an open X11-vs-Wayland-vs-DRM question.

## Docs and tooling map

- **`docs/AGENTS_ARCHIVE_2026-09-10.md`** — everything drained out of this file, each item with its own
  sha/`file:line`: the fork fd boundary and deviations, eligibility verbatim, the per-fork cost history
  and ruled-out causes, the three fork correctness bugs, the correctness-soundness retraction chain, the
  2026-09-08 defect set, the black-desktop root cause, OCI cache internals, the s6-boot filesystem gaps,
  the browser-desktop configuration, crash-dump field semantics and VEH constants, the contested CoW
  flank fix, the `-Dwarnings`/rustfmt rows, and which input-latency PRD items are stale. Older
  narrative: `docs/AGENTS_ARCHIVE_2026-09-03.md`, `_2026-09-05.md`.
- Fork: `docs/track-b-fork-fix-progress.md`, `advisor/ADVISORY-002-d-zero-fork.md`.
  `advisor/ADVISORY-001-fundamentals.md` is the architectural survey — 3N the thread-based fork's tcache
  analysis, Appendix D the presenter case.
- `docs/veh-exception-handler-design.md` — canonical VEH narrative, cited from six places in
  `litebox_platform_windows_userland/src/lib.rs` (`6794160`). Read before touching the handler,
  trampoline or frame sizing.
- Desktop logs: `docs/webtop-debian-selkies-2026-09-06.md`, `docs/webtop-alpine-mate-2026-09-07.md`
  (its 2026-09-10 addendum has the ordered `s6-rc.d` chain and the pixel-bearing 4-service subset),
  `docs/webtop-debian-xfce-2026-09-08.md`, `docs/webtop-xfce-code-vs-data-2026-09-08.md`,
  `docs/fork-fs-veh-2026-09-08.md`.
- Consult before deriving: `docs/premade-library-research.md` (library-vs-hand-rolled audit),
  `docs/drm-dumb-buffer-ioctl-reference.md` (kernel UAPI structs for the dumb-buffer DRM/KMS ioctls
  `litebox_shim_linux/src/syscalls/drm.rs` implements), `docs/diag-timeline-field-semantics.md` (before
  any hypothesis on `DIAG_TIMELINE`'s `comm` field — two investigations mis-traced it).
- `docs/macos.md` — port state and "Remaining work".
  `litebox_platform_macos_userland::guest::run_thread` is a stub and everything blocking it needs real
  Apple Silicon with codesign/JIT-entitlement tooling, so it stays deferred rather than attempted blind
  (PRD rows `macos-aarch64-guest-execution-context-switch-is-not-implemented`,
  `gui-macos-presentation-runner-and-guest-entry-blocked`).
- Probe crates: `docs/wayland-drm-backend-probe/` (a `backend_drm`-only, DumbBuffer-only Smithay
  compositor, musl-cross-built with ziglang + cargo-zigbuild, enumerating litebox's virtual connector and
  CRTC as a real guest process — it surfaced `DRM_IOCTL_MODE_OBJ_GETPROPERTIES`/`GETPROPERTY`, a
  debug-build `--gui` presenter stack overflow, and nested epoll, `1e1da7c`);
  `docs/linux-native-drm-gui-probe/` (the Linux-native DRM → wgpu control case for a Windows-only claim).
- Designs not implemented: `docs/presenter-process-design.md`, `docs/session-daemon-design.md` (its slice
  `litebox_termemu`, a bytes → rendered-screen VT100 emulator, IS implemented; the daemon/IPC layer is
  not), `docs/fork-region-grouping-design.md` (shipped state is still a diagnostic probe).
- `advisor/probes/` — diagnostics (`decode_frame.py`, `symbolize_litebox_crash.py`,
  `cross_process_fork_wait_hang_probe.sh`, `drm_flip_probe.c`, `clone_probe.c`, `run_xfce_xwm.sh`) plus
  three notes worth reading directly: `MEASUREMENT-PITFALLS.md`, `DISK-HYGIENE.md`,
  `fork_verify_third_exec_repro.md`. The OCI-pull Python scripts there are retired.
- `.gm/memories/`: `mem-5ad34546c566b8d6-7306` (RtlpUnwindPrologue), `mem-7cb09e839ca086f2-4223`
  (XFCE/MATE weston, rendering, decoding), `mem-6c4697ac568ea7be-4487` (packager OOM),
  `mem-136ae2ce29bc28a4-3133` (image tags, OCI in-memory loading), `mem-b709a7d784b98110-1430`
  (cross-process sync), `mem-f17269d5777055d3-3326` (2026-09-07 defects), `mem-3e13872ce1ffe95e-2814`
  (CoW), `mem-3c4a9980a884604b-1031` (GUI protocol).
