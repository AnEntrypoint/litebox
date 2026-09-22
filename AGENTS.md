# litebox — current state (2026-09-22)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, and a claim a later commit superseded, were deleted rather than hedged. Reference
detail is drained to the `docs/AGENTS_ARCHIVE_*.md` files and per-investigation logs to the dated
`docs/*.md` in the map below — read those for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one
line plus its pointer, not in a separate memory file. **This file is compacted back under ~30KB
whenever it grows past that threshold** — newest compaction: 2026-09-22 (41KB → this file), full
pre-compaction pass narrative drained verbatim to `docs/AGENTS_ARCHIVE_2026-09-22.md`.

## The cheap repro — start here

```
target/release/litebox_runner_linux_on_windows_userland.exe -Z \
  --oci-image docker.io/library/debian:stable-slim -- /bin/bash -c '<script>'
```

One ~81MB layer, `[cache] HIT` after the first pull, real GNU coreutils instead of busybox — which
matters: coreutils `touch` issues the `utimensat(fd, NULL, …)`/futimens form busybox's never reaches
(`caaac79`). Two host-side gotchas, each already costly:

- **PowerShell, never Git Bash** — Git Bash rewrites `/absolute/guest/paths` into
  `C:/Program Files/Git/...` before the runner sees them, giving a misleading `ENOENT`. `Start-Process
  -RedirectStandardOutput/-RedirectStandardError` makes the runner exit almost instantly with zero guest
  output (no crash dump, no event-log entry); use `& .\runner.exe ... *> combined.log` instead.
- **Single quotes only inside `-c`** — embedded double quotes are corrupted crossing into the child's
  Win32 command line. This masqueraded as deep fork/stack-pointer corruption for a whole sub-session.
- **`LITEBOX_PROCESS_FORK=1` is a HOST env var, not a guest `--env`** — `spawn_cross_process_fork_child`
  (`litebox_platform_windows_userland/src/lib.rs`) reads it via a bare `std::env::var_os` on the HOST
  side; setting it via `--env` instead silently no-ops the whole cross-process path with ZERO log
  output (looks identical to "not eligible", but isn't even attempted) — confirmed live, 37th pass.

**Log level**: default is `warn,litebox_platform_windows_userland::fork_verify=error` (`EnvFilter`'s
own ERROR-only default discarded all real `warn!` sites; `fork_verify` is pinned to `error` because
it warns per single-stepped instruction). Do **not** add `LITEBOX_LOG=error` by reflex; use
`fork_verify=warn` when a fork heal is the subject. A bare `LITEBOX_LOG=debug` (blanket) is
sometimes the fastest way to rule out "which module's silent early-return ate my decision" — cheap
for a small repro, too noisy for a full desktop boot.

## Standing lessons and hard constraints

- **No WSL or hypervisor, ever** — always run under the matching runner
  (`litebox_runner_linux_on_windows_userland.exe`/`litebox_runner_linux_userland`); cross-compiling FOR
  Linux is fine, running the result in a VM defeats the premise.
- **`fork_verify.rs`'s stale-pointer-healing bug class is Windows-only** (real `fork()` gives the
  child identical addresses) — never port to another platform's crate.
- **Never `bcdedit /debug on`** without a kernel debugger already attached — two full-host freezes
  needing a power-cycle.
- **A process spinning inside a dead-locked allocator/spinlock resists `Stop-Process -Force`** —
  use `Invoke-CimMethod -MethodName Terminate` (WMI) instead. `cdb -p <pid>` must use `-pv`/`qd`,
  never a bare `q` (kills the target).
- **Never run two full-stack verifications concurrently** — starves both, looks exactly like a real
  hang. Kill every `litebox_runner` between runs; watch `FreePhysicalMemory`, kill on a falling
  trend not a fixed RSS number.
- **`LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check**, never
  `PrintWindow`/`CopyFromScreen`. A pixel count alone never identifies WHO painted a frame — decode
  frame structure (`advisor/probes/decode_frame.py`) and correlate against `DIAG_TIMELINE execve`'s
  real argv0.
- **Never time litebox with one host process per datapoint** (bare spawn costs 1.6-2.3s) — run N
  iterations inside ONE guest process. Never subtract timestamps across a parent log and a
  fork-child log — `init_logging()` resets elapsed time to ~0 per child.
- **Release-binary `cdb` reads are unreliable** — MSVC linker ICF folds distinct functions into
  one symbol (no `[profile.release]` override exists, so LTO is off but ICF still runs). Build
  `cargo build -p litebox_runner_linux_on_windows_userland` (no `--release`) for any `cdb` session
  needing a trustworthy stack — confirmed live, eighteenth pass, refuted two release-build leads.
- **Refusal errno choice is API contract** — EPERM lets callers degrade, EINVAL/ENOSYS fails them
  hard; wrong choices have silently broken whole subsystems before (archive; 30th pass's AF_UNIX
  `EAGAIN`-vs-`EINPROGRESS` connect() fix is the newest instance).
- **Proving a run took the cross-process fork path needs `[process_fork_diag] task-resume-probe` lines,
  never the shim's eligibility log** — that log fires regardless of whether the fork actually happened
  that way (produced two recorded false conclusions, archive; a THIRD instance this session: an
  `LITEBOX_PROCESS_FORK=1` passed as `--env` instead of a host env var produced an "eligible" shim log
  with no spawn at all and no warning either — see the cheap-repro gotcha above).
- **An fd subsystem being "uncarriable" across a cross-process fork does not mean the fork must be
  refused over it** — only means that ONE fd can't be carried. Pipes/regular files/eventfds are
  carried; close-on-exec and (as of the 37th pass) pty fds are safely DROPPED and the fork proceeds
  anyway; only genuinely un-recoverable kinds (unix-socket, etc.) should still refuse. Check
  `try_cross_process_fork`'s match arms (`litebox_shim_linux/src/syscalls/process.rs`) before
  assuming a new fd kind needs the same treatment as an older, less-understood one.
- **fork carries pipes, regular files and the writable layer into a child, but NOT sockets or ptys
  by fd** — a pre-fork-created listening socket serves nothing to a forked child; a pty fd is
  dropped too, but (unlike a socket) is safely RE-OPENABLE by id afterward via `SharedPtyTable`
  (37th pass) — run dbus-daemon non-forking, and for XFCE use `xfce4-session`, never `startxfce4`.
- **On host-side crashes, use `advisor/probes/symbolize_litebox_crash.py`, snapshotting `.exe`+`.pdb`
  next to the log** — a ring dump's `rva=` is only meaningful against the exact emitting build.
- **Isolate the harness before blaming litebox** — launch guest probes directly as the runner's
  top-level program, never via a runtime-built `/bin/sh -c` wrapper.
- **Never trust a container tag name for its WM/session contents** — verify by registry manifest +
  blob tar-listing, or a live in-guest `/usr/bin` listing.
- **Never record a test count not watched run to completion**; never leave a suite red for an
  environmental reason.
- **Repo hygiene** — packed layer tars, frame dumps and debug logs never go in git (`.wfgy/`,
  gitignored); untrack anything `git add -A` sweeps in.
- **A freestanding no-libc probe's local `char buf[N] = "literal"` array initializer can crash** —
  clang `-O1` lowers it to an aligned SSE `movaps`, and a hand-written `_start` doesn't always give
  the same alignment guarantee real crt0 does. Use a manual byte-copy loop instead (37th pass,
  `advisor/probes/pty_fork_probe.c`'s own `copy_str`).
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

**Eligibility** — an already-borrowed fd table, a beyond-stdio fd that isn't a pipe end/path-recorded
regular file/eventfd/close-on-exec/pty (overridable by `LITEBOX_PROCESS_FORK_IGNORE_FDS`), or an
unsanitizable `fs_base`/context. No by-name gate exists (confirmed by static reading, 34th pass) — the
only gate is this global opt-in env var plus the per-fork fd-kind scan. On a real `debian-xfce` boot
the only remaining blocking kind is `unix-socket`. Per-kind deviations: archive.

**Per-fork cost** was ~3.5-5s, now ~1.2s; `NGINX_STARTED` in under a minute versus never in 15+.
Use `LITEBOX_DIAG_FORK_TIMING=1` for the next cost question.

**Still open**: nginx's own SSL-cert generation fails on its first real startup attempt — the
original symptom this investigation began from, genuinely not root-caused
(`docs/track-b-fork-fix-progress.md:146-152`).

**Pass history (4th-37th, 2026-09-17/22), full narrative for every pass: `docs/
AGENTS_ARCHIVE_2026-09-17.md` (4th-11th), `_2026-09-18.md` (12th-34th, shared AF_UNIX connection
plane; DBUS_FAILED/DISPLAY-getenv()/AF_UNIX-errno CLOSED; Xvfb's SIGSEGV root-caused to a glibc
memcpy reading an unmapped pointer; ldconfig static-PIE double-relocation SIGSEGV fixed), and
`_2026-09-22.md` (26th-37th, verbatim pre-compaction text: `XVFB_UP` fix, `SharedFilePublishTable`
closing `DBUS_FAILED`, AF_UNIX errno fix, the Xvfb crash characterization, the "Fork-after-Xorg
freeze" risk confirmed gone, and the full `SharedPtyTable` design+verification narrative).** The
CURRENT STATE those passes converged on:

- **`DE_FAILED` (`xfce4-session`'s own `Cannot open display: .`) is the sole real blocker on BOTH
  fork paths**, narrowed to "something inside `xfce4-session`'s own process" — envp, `getenv()`,
  and the ELF loader's stack-building code are all proven correct by direct live evidence
  (LD_PRELOAD interposer, 19/19 correct `DISPLAY=:1` reads across a full boot, including inside
  GDK's own backend probe). Never attempted by any pass to date: a live `cdb -pv` attach on
  `xfce4-session` itself, breaking on `getenv`/`XOpenDisplay`/`_XConnectXCB`, only once host RAM is
  genuinely quiet (~1.8GB+ sustained — every attempt below that has died pre-Xvfb to the
  thread-fork tcache-corruption class before ever reaching `xfce4-session`). Treat the recurring
  `Cannot open display: .` text as this GTK/Xt build's generic connection-failed fallback message,
  not literal proof of `getenv("DISPLAY")`'s real return value — that string recurs for unrelated
  root causes elsewhere in this project's own history (archive).
- **Xvfb's own real crash is root-caused but not fixed**: a separate, deterministic SIGSEGV
  ~190-207s into every full boot (not the `DE_FAILED` cause) — glibc's AVX2 memcpy/memmove reading
  64+ bytes from a wild, fully-unmapped pointer (`0x7feffecdd400 == TASK_ADDR_MAX - 0x1312C00`,
  bit-identical `rip`/fault-address across boots). `cdb` attach is REFUTED as a capture method (it
  perturbs the exact X11-traffic race the crash needs) — use `LITEBOX_DIAG_FATALDUMP=1` instead.
  Exact call site still open: needs real CFI-based stack unwinding or an upstream Xvfb/glibc source
  cross-reference (`debuginfod.debian.net` is reachable).
- **The "Fork-after-Xorg PERMANENT freeze" risk is CONFIRMED GONE** (35th pass, live evidence: a
  full `LITEBOX_PROCESS_FORK=1` release-binary boot reached its designed idle `HOLD` loop with zero
  freeze/SIGSEGV/tcache-corruption). `LITEBOX_PROCESS_FORK=1` is now the RECOMMENDED flag for
  `.wfgy/webtop_stack.sh`, evidenced-safe-pending-reconfirmation under workable host RAM. That same
  pass rewrote the `SELKIES_PORT_UP`/`NGINX_SELFTEST` readiness polls from a 170x-`curl`-fork loop
  (which was paying the full cross-process rootfs-rebuild cost per iteration, the likely OOM-kill
  cause) to a zero-fork bash `/dev/tcp/HOST/PORT` builtin check — `bash -n`-clean, not yet
  live-verified end to end.
- **`pty_registry`/`daemon_pty_masters` cross-process redesign is DONE and genuine cross-process
  pty I/O is now LIVE-PROVEN** (36th pass designed+implemented `syscalls::pty::SharedPtyTable`;
  37th pass proved it live and fixed two real bugs the 36th pass's own local-only testing had not
  surfaced). See "Shared-memory foundations" below for the mechanism and `docs/
  AGENTS_ARCHIVE_2026-09-22.md` for the full 37th-pass narrative including the exact repro command
  and log evidence. In short: `advisor/probes/pty_fork_probe.c` (freestanding, host-cross-compiled)
  opens `/dev/ptmx`, forks under `LITEBOX_PROCESS_FORK=1` (set as a HOST env var), and has the
  CHILD — a genuinely separate Windows process (`[process_fork_diag] task-resume-probe (child,
  winpid=...)`) — open `/dev/pts/<id>` fresh and read a marker the PARENT wrote to the master
  strictly after `fork()` returned. Two real bugs were found and fixed to get there, both now
  covered by the "Standing lessons" bullets above: (1) `try_cross_process_fork`
  (`litebox_shim_linux/src/syscalls/process.rs`) refused the ENTIRE fork over any open pty fd
  instead of dropping it like a close-on-exec fd — fixed by adding a `dropped_pty` arm; (2)
  `PtyStateRef::Local`'s setters (`litebox_shim_linux/src/syscalls/pty.rs`) never mirrored
  `TIOCSPTLCK`/termios/winsize/fg_pgid/packet-mode changes into the `SharedPtyTable` slot, so a
  real unlock on the local master left a cross-process `pts_open` seeing the pty as permanently
  locked (`EIO`) — fixed by widening `PtyStateRef::Local` to carry the shared table too and mirror
  every mutation, matching the byte-write path's existing mirror contract.

### Track B — current pickup list, precise (full pass-by-pass evidence: archive)

Fully DONE (kept only as a marker so a future pass doesn't re-attempt): the minimal isolated
cross-process AF_UNIX repro; the `Network` shared-arena redesign's `socket_set`/
`LocalPortAllocator`/`closing_in_background`/`queued_for_closure` slice; DISPLAY/`getenv()` as the
`DE_FAILED` cause (REFUTED FOR GOOD); AF_UNIX `connect()` `EAGAIN`-vs-`EINPROGRESS`; `pty_registry`/
`daemon_pty_masters` (`syscalls::pty::SharedPtyTable`, now live-verified cross-process, 37th pass).

**Open, in rough priority order:**

1. **`xfce4-session`'s `DE_FAILED`** — see above; needs a live `cdb -pv` attach under quiet RAM.
2. **Xvfb's own SIGSEGV call site** — see above; needs CFI unwinding or upstream source
   cross-reference; use `LITEBOX_DIAG_FATALDUMP=1`, never `cdb`, to capture it.
3. `SafeZoneAllocator`'s `spin::mutex::SpinMutex` (`litebox/src/mm/allocator.rs`) needs the same
   dead-holder-recovery treatment `RawMutex` already has — live-caught spinning forever in
   `dealloc`, high blast radius, own dedicated pass.
4. Debugger-root-cause `litebox/src/event/wait.rs:224`'s `unreachable!()` on garbage thread state
   (dozens per boot, most frequent panic historically, NOT yet debugger-confirmed — do not patch
   blind).
5. `SharedUnixConnectQueue`'s cancel-on-first-non-blocking-miss gap (`UnixStreamState::
   Connecting(request_idx)` needed) — scoped, not yet fixed.
6. `flock_registry`/`drm`/`evdev` (`GlobalState` fields, eighteenth-pass audit) remain open — same
   non-POD-payload (Arc-based state, `Pollee` observer lists) obstacle the pty fix's own
   `PtyEnd::Shared{Master,Slave}`/`SharedPtyTable` pattern now gives a concrete template for, not
   yet applied to any of the three; not on the Xvfb/selkies boot path so lower urgency.
7. After the above, `timerfd`/`signalfd` are the next-cheapest carriable fd kinds before attempting
   `socket`/`unix-socket`/`epoll` (`pty` itself is no longer purely uncarriable — a cross-process
   opener can re-acquire a pty by id via `pts_open`'s shared fallback even though the fd itself
   still isn't carried across `fork()`).
8. The general writable-layer-visibility gap for LARGE/unbounded content (`/tmp/de.log`/
   `/tmp/de2.log`/`/tmp/xvfb.log`, `/tmp/wm1`/`/tmp/wm2`) remains open —
   `SharedFilePublishTable`'s 256-byte cap must NOT be widened to try to cover it; a real fix needs
   its own design (chunked publish, or a genuine shared-arena ring rather than a snapshot table).

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

A trampoline-extension failure used to poison a whole segment's syscalls, now fixed (archived).
Tags verified live, never from the name (archived): `linuxserver/webtop:alpine-mate` ships MATE not
XFCE; `alpine-xfce` doesn't exist; `debian-xfce`/`ubuntu-xfce` ship real XFCE.

**X server choice**: for the DRM/wgpu on-screen (`--gui`) path use `Xorg` with `modesetting` — litebox's
virtual DRM device is legacy-KMS + dumb-buffer + XRGB8888 only, no atomic modeset/GBM/EGL, so a GBM-first
compositor lands on its least-tested fallback, and `Xvfb` never touches DRM/KMS at all (zero page-flips,
indistinguishable from "never drew"). For browser/selkies, `Xvfb` IS correct and verified — its
`-shmem` framebuffer works now that SysV shared memory exists.

**Durable artifacts**: `C:\dev\litebox-webtop\webtop_seatd.tar` (stock MATE webtop); the
`.wfgy/xfce-build/` weston+XFCE tar is superseded by the stock-image path.

## A real desktop renders in a browser

**XFCE renders in a real host browser, and MATE too** — full pipeline (Xvfb, selkies/pixelflux
x264, MIT-SHM) inside litebox, reverse proxy host-side only. Working config: selkies
`--addr=0.0.0.0` port **8081**, dashboard over `--publish`, `/websockets` tunnelled to 8081.
Fourteen litebox defects got here, all landed (archive).

**A stock s6-overlay image boots with no flags/stubs**: `/init` runs 16 cross-process children
with zero uncarriable fds. **The black XFCE desktop was deterministic, now fixed**: the runtime
rewriter corrupted `libLLVM.so.19.1`'s `.dynsym`, so mesa `dlopen` failed forever. Rest settled
in the archive (PI futexes, labwc SIGABRT, gdk-pixbuf, network fixes).

**XFCE also renders on the THREAD-based fork path, gated by one flag** (`docker.io/linuxserver/
webtop:debian-xfce`, `.wfgy/webtop_stack.sh`). Without it, 3/3 boots die ~7s in — ADVISORY-001 §3N's
safe-linked-tcache write (the `fork_verify: stale CODE pointer` warning right before it is a red
herring; translation is correct). Fix: `--env GLIBC_TUNABLES=glibc.malloc.tcache_count=
0:glibc.malloc.mxfast=0` as an `--env` runner flag (a bare host-shell prefix does NOT reach the guest
— see the `LITEBOX_PROCESS_FORK` gotcha above for the general form of this mistake: `--env` is
GUEST-side, a bare host `$env:`/`export` is HOST-side, and the two are read by entirely different
code, sometimes silently). Glibc-only workaround, not a fix (PRD
`glibc-tunables-workaround-pending-zero-fork`) for the THREAD-based path specifically.
**`LITEBOX_PROCESS_FORK=1` removes that whole crash class by construction** (no relocation, so no
safe-linked-pointer mistranslation is possible) **and no longer hits the old "Fork-after-Xorg"
freeze either** (35th pass) — the real current blocker on EITHER fork path is `DE_FAILED` (see
above), not AF_UNIX. Selkies also needs `--clipboard-enabled=false` on the thread-based path (its
clipboard monitor re-triggers the same corruption every tick) — moot on the cross-process path for
the same reason `GLIBC_TUNABLES` is.

**Sixth/seventh pass (closed)** — writable-layer-adoption race fixed via the existing
atomic-rename primitive; live-verified 5/5 boots, zero recurrence. Detail: archive.

**Open here.** One client per selkies instance, no slot reclaim on reload. The AF_UNIX
connection-data-plane gap this paragraph used to describe is DONE and crash-free through
`DE_LAUNCHED` — see the Cross-process fork section above for the current sole blocker (`DE_FAILED`).

**The glibc/tcache crash class still sporadically hits selkies** on the THREAD-based fork path, a
SECOND corruption signature under heavy fork load (`double free or corruption (out)` SIGABRT).
Track B territory, not a tunable-coverage gap — do not re-attempt a `GLIBC_TUNABLES` fix without
evidence of a THIRD mechanism. Detail: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

### The ACK-stall-kill and port-8081 watchdog — both CLOSED (2026-09-16)

Real blocker was the guest-side patcher silently crashing on `shutil.copy2()`'s `copystat()` (no
`listxattr` shim) before ever patching `selkies.py`; fixed (`478e640`). Port-8081 double-bind fix
live-verified over 17 boot cycles + a 6000-connection stress test, zero recurrence. Detail: `docs/
AGENTS_ARCHIVE_2026-09-16.md`.

## Host-side crash machinery

A fatal host fault dumps before it dies, ungated (stack walk, `RECENT_FAULTS` ring, `RECOVERY_LOG`,
no env var needed); a real OS minidump comes only from the repeated-identical-fault circuit breaker
(`error_code` is synthesized, `is_in_guest` is tri-state — exact semantics: archive). An unexplained
`0xC0000005` may be a panic — the VEH handler enters only for the four codes it triages, registers
FIRST in the chain, sizes per-depth frames from disassembly not guesswork (`docs/veh-exception-
handler-design.md`). Cross-process sync on Windows is a hard platform constraint: every native
address/TID-based wait is process-local (`WaitOnAddress`, keyed events, `NtAlertThreadByThreadId`=
ACCESS_DENIED); only a shared kernel object crosses processes — `RawMutex` (below) is the one that
matters; `xproc_sync.rs`'s named-event primitive is live-verified but still unwired.

## Shared-memory foundations -- all DONE, live-verified 2026-09-16/17/22 (detail: those archives)

`RawMutex`: no longer calls `WaitOnAddress`/`WakeByAddressSingle` (process-local per MSDN) -- manual
wait queue + one auto-reset kernel `Event` per OS thread, cross-process half real
(`DuplicateHandle`-based); gained `poisoned: AtomicBool` + owner-death recovery; `resolve_waiter_event`'s
stale-pid panic fixed via a fixed-32-slot pointer-free `WaiterQueue`. Shared kernel heap: a small
**64 MiB, standalone, bounded** `shared_kernel_arena_alloc` (`lib.rs`, NOT wired to `GlobalAlloc`)
backs `SharedArc<T>` (`value`+`strong: AtomicUsize`) for `LiteBoxX`/`GlobalState` placement;
`SLAB_ALLOC` stays on the private per-process path (a shared bump allocator exhausted an 8 GiB pool
in 45-90 execs).

**Root cause of the whole `GlobalState`-sharing class, precisely characterized**: `SharedArc::new`
shares only `T`'s literal inline bytes -- any REGISTRY that was a `BTreeMap`/similar has its NODES
on the private per-process heap, meaningless to an attaching process (PRD:
`globalstate-nested-collections-not-actually-shared`). Of the original list (`unix_addr_table`,
`pty_registry`, `daemon_pty_masters`, `flock_registry`, `fifo_registry`, `sysv_shm`, `memfds`,
`shared_files`): `unix_addr_table`/`fifo_registry`/`memfds`/`shared_files` are now
per-process-shadowed on `GlobalStateHandle`, `sysv_shm` is a real shared-arena-native fixed array.
`pty_registry`/`daemon_pty_masters` are ALSO now per-process-shadowed, with a genuine cross-process
companion restored separately via `syscalls::pty::SharedPtyTable` (a plain `GlobalState` field, same
pattern as `unix_addr_presence` below) rather than shadowed away, since real cross-process pty
visibility is actually needed (real devpts semantics: "any process that knows the id can open it") —
**this table is now live-verified genuinely cross-process** (37th pass: a separate Windows process
opened a pty by id and read bytes a different Windows process wrote after `fork()`, over the table's
`SharedByteRing`s; two real mirroring bugs found+fixed to get there, see the Cross-process fork
section above). `flock_registry` remains open (pickup list) — same non-POD-payload obstacle the pty
fix's own pattern (fixed POD control state + `SharedByteRing` data plane, no raw `Arc`/pointer ever
crosses the process boundary) now gives a concrete template for.

`SharedUnixAddrPresenceTable` (`syscalls/unix.rs`) established the reusable flat-table PATTERN
(`sysv_shm`, the AF_UNIX connection-DATA layer, and `SharedPtyTable` all reused it next): fixed-slot,
pure-atomic, lock-free `(kind, key bytes<=108, owner pid)` side-index, or a POD-control-state
variant for a table like `SharedPtyTable` that also needs mutable per-slot fields. `Pipes.litebox`'s
stale pointer (fixed via `GlobalStateHandle::pipes()`) and `FutexManager`'s stack-allocated
`LoanList` sharing hang (fixed: each process gets its own fresh `FutexManager`) were two more
instances of the same raw-pointer-frozen-into-shared-bytes root pattern. **A mutable-state table
built on this pattern still needs every WRITE path individually audited for shared-side mirroring**
— `SharedPtyTable`'s own control-state setters (`set_locked` etc.) were originally wired ONLY into
the local `Arc`-backed side and silently never reached the shared slot until the 37th pass's live
cross-process test caught it (see above); a table with read-only/publish-once shared state (like
`SharedUnixAddrPresenceTable`) doesn't have this failure mode, but any table modeling ongoing
mutable control state does. `SafeZoneAllocator::alloc`'s spinlock livelock (distinct mechanism, no
dead-holder recovery unlike `RawMutex`) is still open.

## Closed — do not re-attempt without a genuinely new approach

VEH_FRAME_STRIDE canary guard, `dev_bench`/`litebox_runner_snp` build failures, CoW-mmap
performance, input-latency bugs, presenter-split duplicate-`SYN_REPORT`, the GUI-protocol
decision, five cheap-wins PRD rows, cross-process-fork stdio-handle bug (`spawn_suspended`'s
clobbered `STARTF_USESTDHANDLES`), presenter-process split (`docs/presenter-process-design.md`)
— all CLOSED, none open. Full detail: archive.

## Docs and tooling map

- **Archives** (newest first) — `_2026-09-22.md` (26th-37th passes, verbatim pre-compaction text:
  `XVFB_UP` fix, `SharedFilePublishTable`/`DBUS_FAILED`, AF_UNIX errno fix, Xvfb crash
  characterization, "Fork-after-Xorg freeze" confirmed gone, and the full `SharedPtyTable`
  design+live-cross-process-verification narrative), `_2026-09-18.md` (12th-34th passes, full
  trace/repro detail — shared AF_UNIX connection plane; DBUS_FAILED/DISPLAY-getenv()/AF_UNIX-errno
  all CLOSED; Xvfb's real SIGSEGV root-caused; ldconfig static-PIE double-relocation SIGSEGV
  fixed), `_2026-09-17.md` (shell-crash investigation, stdio-handle bug, 12 registry/pointer/lock
  fixes, writable-layer-race fix), `_2026-09-16.md` (popup-menu re-test, Track A audit,
  RawMutex/presenter), `_2026-09-15.md` (ACK-stall-kill), `_2026-09-10.md` (fork fd eligibility,
  OCI cache, s6-boot, browser config, crash-dump/VEH, CoW). Older: `_2026-09-03.md`, `_2026-09-05.md`.
- Fork: `docs/track-b-fork-fix-progress.md`, `advisor/ADVISORY-002-d-zero-fork.md`,
  `advisor/ADVISORY-001-fundamentals.md` (§3N tcache). `docs/veh-exception-handler-design.md` —
  read before touching VEH.
- Desktop logs: `docs/webtop-debian-selkies-2026-09-06.md`, `webtop-alpine-mate-2026-09-07.md`,
  `webtop-debian-xfce-2026-09-08.md`, `webtop-xfce-code-vs-data-2026-09-08.md`, `fork-fs-veh-2026-09-08.md`.
- Consult before deriving: `docs/premade-library-research.md`, `docs/drm-dumb-buffer-ioctl-reference.md`,
  `docs/diag-timeline-field-semantics.md` (before any `DIAG_TIMELINE` `comm`-field hypothesis).
- `docs/macos.md` — Apple Silicon guest-execution context switch is a stub, deferred. Designs NOT
  implemented: `docs/session-daemon-design.md`, `docs/fork-region-grouping-design.md`.
- `advisor/probes/` — diagnostics (`decode_frame.py`, `symbolize_litebox_crash.py`, `dup_probe.c`,
  `drm_flip_probe.c`, `clone_probe.c`, `socketpair_fork_probe.c`, `pty_fork_probe.c` — cross-process
  pty I/O verification) plus `MEASUREMENT-PITFALLS.md`, `DISK-HYGIENE.md`.
- `.gm/memories/` — older per-topic notes, superseded by this file/archives.
