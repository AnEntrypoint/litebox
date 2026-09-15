# litebox — current state (2026-09-15)

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
  time twice: weston's own panel mistaken for XFCE's, and panel-shaped pixels attributed to
  `xfce4-panel` on an image that ships MATE and has no `xfce4-panel` at all.
- **Never time litebox with one host process per datapoint** — a bare spawn costs 1.6-2.3s, dwarfing real
  per-exec differences. Run N iterations inside ONE guest process, hold host load constant, and establish
  a noise floor (10+ runs): this host's ~20-36% spread retracted several single-shot "findings". Never
  subtract timestamps across a parent log and a fork-child log — every child is a fresh re-exec whose
  `init_logging()` resets elapsed time to ~0 (`4b95600`).
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
  callers. **`2ce9ba8`** then swept the last two host-killing arms off `layered.rs`'s guest-reachable
  paths.
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
ADVISORY-001 section 3N's tcache corruption is the **thread-based** path's defect only (that conclusion
was reached, retracted and re-reached; chain in the archive). The `fd_complexity.beyond_stdio == 0` check
older notes call the gate is not one — `b2c89fc` found it emitting something false 67 times a boot.

**Eligibility** — refused only for `comm` == `Xvfb` or `dbus-daemon` (`process.rs:2560`, `564af3f` — a
live unix listening socket cannot be served from a fork-time filesystem snapshot), an already-borrowed
fd table, a beyond-stdio fd that is not a pipe end / path-recorded regular file / eventfd / close-on-exec
(overridable by `LITEBOX_PROCESS_FORK_IGNORE_FDS`), or an unsanitizable `fs_base`/context. On a real
`debian-xfce` boot the only remaining blocking kind is `unix-socket` — 5 refused forks of 34, down from
34/34 (`ed74c28`). The per-kind deviations that matter (file offsets and eventfd counters are **copied,
not shared**; CLOEXEC fds are dropped, so a child using one before `execve` gets `EBADF`) are in the
archive — read it before assuming a guest gets real `fork()` sharing semantics.

**Per-fork cost** was ~3.5-5s, now ~1.2s (`1199ab6`, `cc7986f`, `ce5648f`), and a full `webtop_stack.sh`
boot reaches `NGINX_CONFIGURED`/`NGINX_STARTED` in under a minute versus never in 15+. The
rootfs-re-merge, `WindowsUserland::new()` and writable-layer-growth explanations older notes give are
**measured wrong** and ruled out by name (`78040e3`, `6d4248b`/`c208e12`). Use `LITEBOX_DIAG_FORK_TIMING=1`
for the next cost question. Three correctness bugs that work exposed are all fixed — `060ccc3` (no
`SIGCHLD` reached a cross-process child's parent), `6e86a40` (`sys_wait4(pid=-1)` skipped the
cross-process registry), `d5cc744` (a redundant claim release deleted a coalesced `CLAIMED_RANGES` slot);
mechanisms, repros and the cost-measurement history: archive.

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
Silicon hosts only, `:114`). It supersedes the ad-hoc OCI-pull Python scripts this project once
hand-rolled — retired, do not recreate.

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

**A trampoline-extension failure used to poison a whole segment's syscalls, now fixed** (`6311f74`). The
initial trampoline allocation was a flat one-page guess; a segment needing more stub space (a few hundred
`syscall` sites — ordinary for a real binary) then tried to extend at one fixed adjacent address with no
fallback, and any unrelated mapping there made `apply_trap_fallback` poison **every** syscall in the
segment with `ICEBP;HLT`, killing the guest on its first syscall after load. The allocation is now sized
from a cheap `0F 05` byte-pair count (sound upper bound), capped at 4MiB. Witnessed live on a fresh
`--oci-image` pull of `docker.io/edgelevel/alpine-xfce-vnc:latest`: busybox `/bin/sh` SIGILL within 3s
before, zero fatal signals after. Any real binary whose patchable-syscall count exceeded one page of
stubs was exposed, not just that image.

**Tags, verified live, never from the name**: `linuxserver/webtop:alpine-mate` ships MATE, not XFCE;
`alpine-xfce` does not exist (404); `debian-xfce`/`ubuntu-xfce` DO ship a real XFCE stack (`34da133`,
`c65ab93`, `1ea5203`; XFCE ships only on the debian/ubuntu/fedora/arch bases, `8c07f51`). The `alpine-*`
flavors share one ~519MB base layer (`9c7ea2b`); `debian-xfce` is a 17-layer Debian 13 image sharing
nothing with them. `edgelevel/alpine-xfce-vnc` is Alpine 3.16.0 with `Xvfb`/`x11vnc`/`novnc_server` plus
a full `xfce4-session`/`xfwm4` set — a noVNC-over-browser pipeline in the already-settled Xvfb category.
`ubuntu-xfce` packs fine but its rust-coreutils aborts in rustix auxv handling, taking out `sleep`/`tail`
and the DE launch — `bb46f1a` has since implemented `/proc/self/auxv` and `AT_EXECFN`, so that is one
re-test, not a fresh investigation (archive).

**Which X server**: for the DRM/wgpu on-screen (`--gui`) path use `Xorg` with `modesetting` — litebox's
virtual DRM device is legacy-KMS + dumb-buffer + XRGB8888 only, no atomic modeset, no GBM/EGL, so a
GBM-first compositor lands on its least-tested software-rendering fallback, and `Xvfb` never touches
DRM/KMS there at all (zero page-flips, indistinguishable from "the guest never drew"). For the
browser/selkies path `Xvfb` IS correct and verified: `alpine-mate`'s own `svc-xorg/run` execs
`/usr/bin/Xvfb` (`/usr/bin/Xorg` is a 275-byte sh wrapper no service invokes), and its `-shmem`
framebuffer works now that SysV shared memory exists (`4abf971`).

**Durable artifacts**: `C:\dev\litebox-webtop\webtop_seatd.tar` (stock MATE webtop);
`.wfgy/xfce-build/layer31_direct_fixed.tar` (hand-assembled weston+XFCE, superseded by the stock-image
path).

## A real desktop renders in a browser

**The XFCE desktop renders in a real host browser** — Chrome on the host showing xfdesktop's icons, the
cursor and live H264 pixels off the guest's X server, reproduced twice on a fresh stack (`f10c5e9`). The
whole pipeline (Xvfb, X clients, selkies/pixelflux x264, MIT-SHM capture) runs inside litebox; only the
reverse proxy is host-side. **A default-configured MATE desktop also renders**, panels and menus and
input round-trip included (`8c07f51`, `181ec68`). The exact working configuration — selkies
`--addr=0.0.0.0` on port **8081**, dashboard over `--publish`, `/websockets` tunnelled to 8081 — is in
the archive, along with what was actually on screen. Getting there took two sets of seven independent
litebox defects, all landed (memory `mem-f17269d5777055d3-3326` for the 2026-09-07 set, whose final
blocker was `4abf971` SysV shared memory since pixelflux capture needs MIT-SHM; the 2026-09-08 set is
enumerated in the archive).

**A stock s6-overlay image boots with no flags and no stubs**: `/init` on `webtop_seatd.tar` runs 16
cross-process children with zero uncarriable fds and zero fatal errors, all the way into supervision
(`66bd640`, `docs/fork-fs-veh-2026-09-08.md:35-37`) — retiring three "fundamental blocker" claims older
notes carried (s6 `/init` ET_EXEC collision, only-one-`python3`, `failed to map segment` under load).

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

**The XFCE desktop renders on the THREAD-based fork path too, and one flag is why** (2026-09-15,
`docker.io/linuxserver/webtop:debian-xfce`, `.wfgy/webtop_stack.sh` over `--publish 3000:3000`). Without
it this boot dies 3/3 at a fixed point ~7s in — `comm=sh`, pre-execve, in the fork child taken right
after the nginx self-test — and `XVFB_FAILED`/`DE_FAILED` follow because the X server never comes up.
The register capture identifies that fault as ADVISORY-001 section 3N instruction-for-instruction
(`__libc_malloc+0x76`'s `xor (%rax),%rsi`: fault address `== %rax`, `%rsi == %rax >> 12`, safe-linking's
own `pos>>12` term), and the `fork_verify: stale CODE pointer ... translating and resuming` warning that
precedes it is a red herring — the identical instruction re-faults on the identical address at the
translated `rip`, so the translation is byte-correct. The flag:
`--env GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0`. It turns off glibc's only two
safe-linked freelists; what remains links chunks with plain `fd`/`bk` pointers that relocation healing
already handles. Section 3N predicted `tcache_count=0` alone would merely move the fault to the
fastbins; `mxfast=0` is the half that closes it. Workaround, glibc-only, not a fix (PRD row
`glibc-tunables-workaround-pending-zero-fork`) — `LITEBOX_PROCESS_FORK=1` still removes the class
properly but cannot run a desktop until the AF_UNIX gap below closes. Selkies additionally needs
`--clipboard-enabled=false` (its clipboard monitor fork+execs `xclip` on a timer, which is a fresh draw
against the same corruption every tick; it took the stream down at rc=139). Live browser witness of the
full result, with input round-trip and an advancing panel clock: memory `mem-e5107049137fcf43-1303`.

**Open here.** Selkies serves one client and a page reload does not reclaim the slot, so a fresh stack
is needed per view. An intermittent host AV ends some runs at varying points (latest shape `rip == fault
address == 0x7ff003444000`, an instruction fetch in the host-allocator region) — that, not the desktop
background, is this area's real remaining non-determinism. The one architectural gap for a full desktop
on the crash-free cross-process path: **guest processes share no AF_UNIX/loopback/FIFO namespace**, so a
cross-process fork gives zero AVs but Xvfb is unreachable from its clients (`/tmp/.X11-unix/X1` is not a
shared object); one host-side transport shared by every process of a guest would put the whole desktop
on the already-crash-free path (`docs/fork-fs-veh-2026-09-08.md:128-144`).

## Host-side crash machinery

**A fatal host fault dumps before it dies, ungated** — the stack walk, `RECENT_FAULTS` ring
(`litebox_platform_windows_userland/src/lib.rs:789`) and `RECOVERY_LOG` print with no env var set
(`lib.rs:1950-1956`); a real OS minidump (`199655b`) comes only from the repeated-identical-fault
circuit breaker, >64 faults at one rip.

**Two dump fields mislead if you trust an old reading.** `error_code` is synthesized here and `19740a3`
made its Present bit REAL rather than hardcoded 0, so **every pre-2026-09-10 "not-present, therefore
unmapped" inference from `error_code` is unsound** and a pre-fix "write" may really have been an
instruction fetch. `is_in_guest` became tri-state in `c0c1472`: before it, a thread whose TLS
identification merely failed printed identically to a confirmed `false`, and `rip`/`rsp` could be torn
across two reads of a live `CONTEXT`. Exact semantics: archive.

**An unexplained `0xC0000005` with no further detail may be something that panicked** — litebox no longer
sees Rust panics as panics. The handler used to carry every exception code, `EXCEPTION_STACK_OVERFLOW` and
the MSVC/Rust panic code `0xE06D7363` included, through a stack swap into a handler with no interest in
it, overflowing the stack; it now enters only for the four codes it triages (`78dda05`) and registers
FIRST in the chain (`5870ab0`). Per-depth frames are sized from disassembly rather than guessed
(`5cacf7f`, `0473cc3`), and `dc108fb` stopped the watchdog killing a successfully-recovered run.
Constants: archive. Narrative: `docs/veh-exception-handler-design.md`.

**Cross-process synchronization on Windows is a hard platform constraint** (measured, memory
`mem-b709a7d784b98110-1430`): every native address/TID-based wait is process-local — `WaitOnAddress`,
keyed events, `NtAlertThreadByThreadId` (ACCESS_DENIED). The only cross-process wake is a shared kernel
object; NAMED auto-reset Events work cross-process with no `DuplicateHandle`. The primitive exists and is
live-verified (`litebox_platform_windows_userland/src/xproc_sync.rs`, `b2166c4`); wiring it into
`RawMutex` is open (PRD row `wire-xproc-sync-crossprocessmutex-into-litebox-platform-rawmutex`).

## Closed — do not re-attempt without a genuinely new approach

**There is no open host crash.** The `RtlpUnwindPrologue` crash earlier notes called "the one genuinely
open" one was `VEH_FRAME_STRIDE`, closed by `0473cc3` on 2026-09-08 (`271cbb5`). Never inside
`fork_verify` — its AV-heal storm (238-714 heals/run) is merely what nests the VEH to `veh_depth=2`,
where the 4096-byte per-level slice was 168 bytes short of the two frames it must cover, so the inner
handler wrote through the outer's live frame and `EXCEPTION_RECORD`. `RtlpUnwindPrologue` was the
secondary, and the UTF-16 `LITEBOX_DIAG_FAT…LT_VEC` was a nested handler's own `env::var_os` buffers
(`8ef49b5`+`fdf5dd9`). Bisected live 2026-09-15 on `mate-session --version` x3 against
`webtop_seatd.tar`: `66bd640`/`b0fe210` 10/10 fatal, `0473cc3` 0/10, `5683a4e` 0/57. Mechanism,
signature and the diagnostic pitfall: memory `mem-c62454fedb1baef8-2714`. Still unguarded, not a live
defect: nothing detects a too-small stride (PRD row `veh-frame-stride-has-no-overflow-guard`).

**Windows CoW-mmap performance.** Zero practical effect on real tar-packed execs: `MapViewOfFile3` needs
64KiB file-offset alignment and real ELF `PT_LOAD` segments are only page-aligned with no exploitable
slack, regardless of tar-file-start alignment. **The `LITEBOX_COW_MMAP` default-off is load-bearing**:
the shipped flank restoration (`329174b`) recommits orphaned flanks as anonymous **zero-fill**, while the
fault it fixed was `ld.so` **reading** that flank's `.gnu.hash`/`.dynsym` — so opting in trades a loud
SIGSEGV for silently zeroed symbol tables. Detail: archive,
`docs/cow-mmap-fixed-address-design.md`, memory `mem-3e13872ce1ffe95e-2814`.

**Input latency.** Three real bugs fixed and verified live: sub-pixel mouse remainders truncated by
float-to-i32, now accumulated losslessly; each physical movement delivered as TWO evdev reports instead
of one, now a single `SYN_REPORT` (`59e4ca0`, live-counted witness in `5683a4e`); the window locked to
the guest's virtual resolution, now resizable with scaled deltas. Present mode is **Mailbox-preferred
with Fifo fallback** (`presentation.rs:1064-1080`, `5a194f5`/`aa1d0ca`) — any note calling it Fifo-only
is stale. Genuinely open: deltas come from differencing winit `CursorMoved` absolute positions rather
than `DeviceEvent::MouseMotion`, so sub-pixel accumulation mitigates quantization but not winit's event
rate (PRD `mouse-motion-devicevent-needs-pixel-calibration`); the linux/macos userland presenters still
emit two reports per move (`linux-macos-userland-presentation-still-emits-two-syn-reports-per-move`);
and there is no framerate baseline, since an idle compositor with no client legitimately produces zero
page flips — a real moving on-screen client first.

**The GUI protocol decision is settled**: DRM/KMS + wgpu, proven live with guest page-flip pixels in a
real host window (memory `mem-3c4a9980a884604b-1031`). Not an open X11-vs-Wayland-vs-DRM question.

## Docs and tooling map

- **Archives**, each with its own headings and shas — `docs/AGENTS_ARCHIVE_2026-09-15.md` (drained today)
  and `docs/AGENTS_ARCHIVE_2026-09-10.md` (fork fd boundary/deviations/eligibility verbatim, per-fork cost
  history, the 2026-09-08 defect set, OCI cache internals, s6-boot filesystem gaps, the browser-desktop
  configuration, crash-dump field semantics and VEH constants, the CoW flank fix, working practices).
  Older narrative: `docs/AGENTS_ARCHIVE_2026-09-03.md`, `_2026-09-05.md`.
- Fork: `docs/track-b-fork-fix-progress.md`, `advisor/ADVISORY-002-d-zero-fork.md`.
  `advisor/ADVISORY-001-fundamentals.md` is the architectural survey — 3N the thread-based fork's tcache
  analysis, Appendix D the presenter case.
- `docs/veh-exception-handler-design.md` — canonical VEH narrative, cited from six places in
  `litebox_platform_windows_userland/src/lib.rs` (`6794160`). Read before touching the handler,
  trampoline or frame sizing.
- Desktop logs: `docs/webtop-debian-selkies-2026-09-06.md`, `docs/webtop-alpine-mate-2026-09-07.md` (its
  2026-09-10 addendum has the ordered `s6-rc.d` chain and the pixel-bearing 4-service subset),
  `docs/webtop-debian-xfce-2026-09-08.md`, `docs/webtop-xfce-code-vs-data-2026-09-08.md`,
  `docs/fork-fs-veh-2026-09-08.md`.
- Consult before deriving: `docs/premade-library-research.md` (library-vs-hand-rolled audit),
  `docs/drm-dumb-buffer-ioctl-reference.md` (kernel UAPI structs for the ioctls
  `litebox_shim_linux/src/syscalls/drm.rs` implements), `docs/diag-timeline-field-semantics.md` (before
  any hypothesis on `DIAG_TIMELINE`'s `comm` field — two investigations mis-traced it).
- `docs/macos.md` — port state and "Remaining work". `litebox_platform_macos_userland::guest::run_thread`
  is a stub and everything blocking it needs real Apple Silicon with codesign/JIT-entitlement tooling, so
  it stays deferred rather than attempted blind (PRD `macos-aarch64-guest-execution-context-switch-is-not-implemented`,
  `gui-macos-presentation-runner-and-guest-entry-blocked`).
- Probe crates: `docs/wayland-drm-backend-probe/` (a `backend_drm`/DumbBuffer-only Smithay compositor,
  musl-cross-built with ziglang + cargo-zigbuild, driving litebox's virtual connector as a real guest
  process — it surfaced `DRM_IOCTL_MODE_OBJ_GETPROPERTIES`/`GETPROPERTY`, a debug-build `--gui` presenter
  stack overflow, and nested epoll, `1e1da7c`); `docs/linux-native-drm-gui-probe/` (the Linux-native
  DRM → wgpu control case for a Windows-only claim).
- Designs not implemented: `docs/presenter-process-design.md`, `docs/session-daemon-design.md` (its slice
  `litebox_termemu`, a bytes → rendered-screen VT100 emulator, IS implemented; the daemon/IPC layer is
  not), `docs/fork-region-grouping-design.md` (shipped state is still a diagnostic probe).
- `advisor/probes/` — diagnostics (`decode_frame.py`, `symbolize_litebox_crash.py`,
  `cross_process_fork_wait_hang_probe.sh`, `drm_flip_probe.c`, `clone_probe.c`, `run_xfce_xwm.sh`) plus
  `MEASUREMENT-PITFALLS.md`, `DISK-HYGIENE.md`, `fork_verify_third_exec_repro.md`, all worth reading
  directly. The OCI-pull Python scripts there are retired.
- `.gm/memories/`: `mem-c62454fedb1baef8-2714` (RtlpUnwindPrologue, resolved — supersedes the
  "unresolved" `mem-5ad34546c566b8d6-7306`), `mem-e5107049137fcf43-1303` (2026-09-15 live browser witness),
  `mem-7cb09e839ca086f2-4223` (XFCE/MATE weston, rendering, decoding), `mem-6c4697ac568ea7be-4487`
  (packager OOM), `mem-136ae2ce29bc28a4-3133` (image tags, OCI loading), `mem-b709a7d784b98110-1430`
  (cross-process sync), `mem-f17269d5777055d3-3326` (2026-09-07 defects), `mem-3e13872ce1ffe95e-2814`
  (CoW), `mem-3c4a9980a884604b-1031` (GUI protocol).
