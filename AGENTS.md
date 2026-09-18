# litebox — current state (2026-09-18)

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
- **Repo hygiene** — packed layer tars, frame dumps and debug logs never go in git (`.wfgy/`,
  gitignored); untrack anything `git add -A` sweeps in.
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
regular file/eventfd/close-on-exec (overridable by `LITEBOX_PROCESS_FORK_IGNORE_FDS`), or an
unsanitizable `fs_base`/context. The old unconditional-by-`comm`-name refusal for `Xvfb`/
`dbus-daemon` is REMOVED as of the twelfth pass (below) — both now go through this same scan like
everything else. On a real `debian-xfce` boot the only remaining blocking kind is `unix-socket` — 5
refused forks of 34, down from 34/34 (pre-twelfth-pass baseline). Per-kind deviations: archive.

**Per-fork cost** was ~3.5-5s, now ~1.2s; `NGINX_STARTED` in under a minute versus never in 15+.
Use `LITEBOX_DIAG_FORK_TIMING=1` for the next cost question.

**Still open**: nginx's own SSL-cert generation fails on its first real startup attempt — the
original symptom this investigation began from, genuinely not root-caused
(`docs/track-b-fork-fix-progress.md:146-152`).

**Passes 4-11 (2026-09-17), all FIXED and live-verified, full narrative in the 09-17 archive**:
(4) nginx-self-test pipe-EOF wedge — broad `bInheritHandles=TRUE` leaked a sibling fork child's
bridge-pipe handle; fixed via `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` explicit handle allow-list. (8)
`Network::socket_set` made shared-arena-native (256-slot fixed array, was a `tuple.unwrap()` panic
via a private-heap `Vec`). (10) post-`NGINX_STARTED` CPU livelock — `LocalPortAllocator`
hashbrown-SIMD-on-dead-heap; converted to a fixed `[u16; 65535]` array (`local_ports.rs`);
`closing_in_background` converted the same way. `interface`/`queued_for_closure` remain open. (11)
poison-on-dead-holder scheme for `Network` — `RawMutex` gained `poisoned: AtomicBool`,
`Network::reset_after_poisoning` wholesale-resets on poison; a removed `Socket`'s `Drop` freeing
another process's heap via its RX/TX ring `Vec`s fixed via `core::mem::forget`.

**Twelfth pass**: by-name `Xvfb`/`dbus-daemon` cross-process-fork exclusion relaxed — both now
cross-process-fork for real. `SafeZoneAllocator::dealloc`'s `spin::mutex::SpinMutex`
(`litebox/src/mm/allocator.rs`), live-caught spinning forever with no dead-holder recovery unlike
`RawMutex` — **still open**.

**Thirteenth pass, 2026-09-18** — shared cross-process AF_UNIX connection data plane DESIGNED and
IMPLEMENTED (`SharedUnixConnTable`/`SharedUnixConnectQueue`, `syscalls/unix.rs`'s module doc has
the full design); three real bugs found+fixed along the way.

**Fourteenth/fifteenth passes, 2026-09-18.** Isolated AF_UNIX repro PASSED clean; the full-boot
stall's first theory (`wait_on_tun` two-holder deadlock) was WRONG (symbol-resolution noise, trust
only small offsets). Real root cause: a smoltcp stale-`SocketHandle` panic killed `net_worker`
threads platform-wide. FIXED via `catch_unwind` + `force_reset_network_after_panic()`.

**Sixteenth pass** — `.wfgy/webtop_stack.sh`'s `[ -S "$XSOCK" ]` readiness check can never be true
on this shim (no `Socket` `FileType` variant), burning its full 60s every boot. Fixed: `-S` → `-e`.

**Seventeenth pass** — `xset q`'s silent kill CAUGHT LIVE and FIXED (`memfds`/`shared_files`,
seventh/eighth instance of the BTreeMap-in-shared-arena defect; `GlobalStateHandle` now carries
fresh-per-process copies). Live-verified twice, zero panics. Full pass 4-17 narratives: `docs/
AGENTS_ARCHIVE_2026-09-17.md` and `_2026-09-18.md`.

**Eighteenth pass, 2026-09-18 -- systematic `GlobalState` field audit (3 more defects fixed), a
debug build for reliable `cdb` symbols, an unambiguous read on the post-xset stall (detail:
archive).** Fixed `unix_addr_table`/`fifo_registry` (per-process-private, shadowed) and `sysv_shm`
(fixed 128-slot array). Still open, deeper redesign needed: `pty_registry`/`daemon_pty_masters`/
`flock_registry`/`drm`/`evdev` (pickup list). Found the real blocked thread: guest `ppoll()`
(`sys_ppoll -> PollSet::wait -> commit_wait -> RawMutex::block_or_maybe_timeout`), parked on a
Condvar, AF_UNIX bounded-15ms-repoll running but the awaited `has_pending(...)` never flips true.

**Nineteenth pass, 2026-09-18 -- live repro of the ppoll stall (twice, debug binary, direct `cdb
-pv` thread-stack evidence), `has_pending`/queue logic REFUTED as the bug, one real structural gap
found and NOT yet fixed (detail: archive).** Booted `.wfgy/webtop_stack.sh` under
`LITEBOX_PROCESS_FORK=1` + debug binary twice; both times independently reproduced the identical
`sys_ppoll -> PollSet::wait -> commit_wait` stack via non-invasive `cdb -pv` on the live guest
process (winpid 17248, then its retry winpid 4700), confirming the eighteenth-pass read is real
and repeatable, not a one-off. **Identity established**: `task.pid.get()` (used as `self_pid`/
`owner_pid` in the `unix_addr_presence`/`[unix_addr_presence]` WARN) IS the real Windows PID for a
`LITEBOX_PROCESS_FORK=1` child -- `owner_pid=9964` in the log directly correlates to Xvfb's own
host process (confirmed via its `task-resume-probe` resume offset, 16877, matching the script's
own `Xvfb ... &` launch line). The connecting client both times was `xset q` (script offset
19289/19468). **`SharedUnixConnectQueue`/`has_pending`/`try_claim`/`complete` code-reviewed
line-by-line: structurally sound** -- no bug found in the matching/claiming logic itself, and
`presence_kind_and_bytes`/`to_key()` produce identical keys on both the `listen()`-side insert and
the `connect_cross_process`-side lookup, so a key-encoding mismatch is also ruled out.
**Real gap found (confirmed via `wait_on_events_polling`/`polling.rs:60-63`'s own nonblock
short-circuit): a NON-BLOCKING cross-process `connect()` that doesn't complete synchronously posts
into `unix_shared_connect_queue` and returns `EINPROGRESS`-equivalent immediately, but the
`request_idx` is never stored anywhere on the socket, and `UnixInitStream::check_io_events` (the
`Init`-state arm, `syscalls/unix.rs` around line 1590-1602) is a static `OUT|HUP` report that never
re-checks `unix_shared_connect_queue.poll_result()` -- so a later `poll()`/`select()`/`ppoll()` on
that same fd can NEVER observe the connection actually completing.** This is real and confirmed by
code reading, but NOT yet confirmed as THE mechanism behind xset's own stall specifically (xset's
Xlib connect is very likely blocking, not non-blocking, so this gap more plausibly explains a
LATER dbus/xfce4-session-class client than xset itself) -- **not fixed this pass, deliberately**:
repeated attempts to catch the exact stuck thread's `PollSet` `entries` (fd/mask) via a second,
immediate follow-up `cdb -pv` call raced the process's own teardown every time (it reliably
self-terminates within 1-3 minutes of being caught, before a second attach could complete) so the
"which fd/direction" question named by the eighteenth pass is STILL not conclusively answered --
see the pickup list below for the precise next step. Separately ruled out: `13448` (nginx
supervisor, blocked in a legitimate wait4 on its live-nginx child -- not a bug) and the
`decode_cross_process_wait_status`/`CROSS_PROCESS_EXIT_MARKER` dead-code path (real dead code, but
NOT the live cause here -- the diagnostic `task-resume-probe` harness already encodes
`0xc0de0000` correctly on every NORMAL child exit; xset's "Killed" report is consistent with it
never reaching its own exit path at all, i.e. genuinely still running when killed, not a
misreported normal exit). Full cdb transcripts + timeline: `docs/AGENTS_ARCHIVE_2026-09-18.md`.

### Track B — current pickup list, precise (full pass-by-pass evidence: archive)

(-2) ~~root-cause `net::wait_on_tun`~~ — REFUTED, fifteenth pass (smoltcp panic, FIXED). ~~blocked
by `unix_addr_table`'s connection-DATA sharing~~ — REFUTED, sixteenth pass (`-S`/`-e` script bug,
FIXED). ~~blocked by a fatal kill of `xset`~~ — ROOT-CAUSED and FIXED, seventeenth pass
(`memfds`/`shared_files`). ~~stall past `xset` is in `connect_cross_process`'s rendezvous or
`net::wait_on_tun`/`NatGateway::new`~~ — REFUTED, eighteenth pass (both innocent background
threads). ~~the `has_pending`/`SharedUnixConnectQueue` matching logic itself is buggy~~ — REFUTED,
nineteenth pass (line-by-line review found it structurally sound; live-reproduced the identical
stall twice with fresh, independent `cdb` evidence). **Current best lead, NOT yet fixed**: a
non-blocking cross-process `connect()` that returns `EINPROGRESS` never stores its
`unix_shared_connect_queue` `request_idx` anywhere, and `UnixInitStream::check_io_events`
(`syscalls/unix.rs` ~1590) is a static `OUT|HUP` report that never re-polls the queue for
completion -- confirmed by code reading (`wait_on_events_polling`'s nonblock short-circuit,
`polling.rs:60-63`), NOT yet confirmed live as xset's own specific mechanism (xset's own connect is
likely blocking) nor fixed. **Precise next step**: reproduce once more (`.wfgy/
repro_debug_ppoll_stall.ps1` is the ready-to-run launch script, debug binary + `LITEBOX_
PROCESS_FORK=1` + `.wfgy/webtop_stack.sh` via `--resume-from .wfgy/webtop_seed.tar`), and as SOON
as a process shows the `sys_ppoll -> PollSet::wait -> commit_wait` stack via a `cdb -pv -p <pid> -y
target\debug -c "~*kb;qd"` sweep, IMMEDIATELY (same breath, no gap -- the process reliably
self-terminates within 1-3 minutes of being caught) re-attach and dump `.frame 6;dv /t /v` (frame
index confirmed stable across two independent captures: 00 ntdll, 01 KERNELBASE, 02
`RawMutex::block_or_maybe_timeout`, 03 `block_or_timeout`, 04 `commit_wait`, 05 `wait_until`, 06
`PollSet::wait`, 07 `sys_ppoll::closure$1`, 08 `Task::sys_ppoll`) to read `self.entries` (fd list +
requested mask) -- this is the one piece of evidence still missing. Separately: once a process gets
reaped after being caught stuck, NOTHING continues the boot script afterward (observed: log frozen
indefinitely, no new `task-resume-probe` lines, no `[s]` markers) -- worth checking next session
whether the resume/continuation chain itself drops the next script step when a child is
externally-timed-out rather than exiting normally. Full evidence: `docs/AGENTS_ARCHIVE_2026-09-18.md`,
fifteenth through nineteenth passes.

(-1) ~~Build the minimal isolated cross-process AF_UNIX repro~~ — DONE, fourteenth pass. (0)

(0b) `SafeZoneAllocator`'s `spin::mutex::SpinMutex` (`litebox/src/mm/allocator.rs`) needs the same
dead-holder-recovery treatment `RawMutex` already has — live-caught spinning forever in `dealloc`,
high blast radius, own dedicated pass. (1b) `queued_for_closure`'s own still-open cross-process-Vec
hazard (STORAGE not yet converted to a fixed pointer-free array the way `closing_in_background`/
`socket_set` already were) remains a live risk. (2) debugger-root-cause `litebox/src/
event/wait.rs:224`'s `unreachable!()` on garbage thread state (dozens per boot, most frequent
panic historically, NOT yet debugger-confirmed — do not patch blind); (3) root-cause the
`/tmp/empty` writable-layer cross-child-visibility gap; (4) finish the `Network` shared-arena
redesign (`interface`, `queued_for_closure` remain); (4b) `pty_registry`/`daemon_pty_masters`/
`flock_registry`/`drm`/`evdev` (`GlobalState` fields, eighteenth-pass audit) genuinely need
cross-process visibility per their own doc comments but hold non-POD payload (Arc-based state,
`Pollee` observer lists), so need a deeper redesign than `sysv_shm`'s flat-Copy-slot-array fix —
not yet touched by any pass, not on the Xvfb/selkies boot path so lower urgency than (0)-(4); (5)
after (0)-(4), `timerfd`/`signalfd` are the next-cheapest carriable fd kinds before attempting
`socket`/`unix-socket`/`pty`/`epoll`.

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

**Sixth/seventh pass (closed)** — a writable-layer-adoption race fixed via the existing
atomic-rename primitive; live-verified 5/5 boots, zero recurrence. Detail: archive.

**Open here.** One client per selkies instance, no slot reclaim on reload. Architectural gap:
**guest processes share no AF_UNIX/loopback/FIFO namespace** — precisely confirmed and named
(`unix_addr_table`'s `Backlog`/`Channel` connection data) in the twelfth-pass entry above; extending
that table's presence-sharing pattern to real connection data (now DONE, thirteenth pass, and
proven sound in isolation by the fourteenth-pass repro) was meant to put the desktop on the
crash-free path — the fourteenth-pass full-boot stall is the thing standing between here and there.

**The glibc/tcache crash class still sporadically hits selkies** on the THREAD-based fork path, a
SECOND corruption signature under heavy fork load (`double free or corruption (out)` SIGABRT),
separate from the ACK-stall-kill below. Track B territory, not a tunable-coverage gap — do not
re-attempt a `GLIBC_TUNABLES`/env fix without evidence of a THIRD mechanism. Detail: `docs/
AGENTS_ARCHIVE_2026-09-16.md`.

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

## Shared-memory foundations -- all DONE, live-verified 2026-09-16/17 (detail: those two archives)

`RawMutex`: no longer calls `WaitOnAddress`/`WakeByAddressSingle` (process-local per MSDN) -- manual
wait queue + one auto-reset kernel `Event` per OS thread, cross-process half real
(`DuplicateHandle`-based); gained `poisoned: AtomicBool` + owner-death recovery (eleventh pass,
above); `resolve_waiter_event`'s stale-pid panic fixed via a fixed-32-slot pointer-free
`WaiterQueue`. Shared kernel heap: a small **64 MiB, standalone, bounded**
`shared_kernel_arena_alloc` (`lib.rs`, NOT wired to `GlobalAlloc`) backs `SharedArc<T>`
(`value`+`strong: AtomicUsize`) for `LiteBoxX`/`GlobalState` placement; `SLAB_ALLOC` stays on the
private per-process path (a shared bump allocator exhausted an 8 GiB pool in 45-90 execs).
**Root cause of the whole `GlobalState`-sharing class, precisely characterized**: `SharedArc::new`
shares only `T`'s literal inline bytes -- any REGISTRY that was a `BTreeMap`/similar has its NODES
on the private per-process heap, meaningless to an attaching process (PRD:
`globalstate-nested-collections-not-actually-shared`). Of the original list (`unix_addr_table`,
`pty_registry`, `daemon_pty_masters`, `flock_registry`, `fifo_registry`, `sysv_shm`, `memfds`,
`shared_files`): `unix_addr_table`/`fifo_registry`/`memfds`/`shared_files` are now
per-process-shadowed on `GlobalStateHandle`, `sysv_shm` is a real shared-arena-native fixed array.
`pty_registry`/`daemon_pty_masters`/`flock_registry` remain open (pickup list).
`SharedUnixAddrPresenceTable` (`syscalls/unix.rs`) established the reusable flat-table PATTERN
(`sysv_shm` and the AF_UNIX connection-DATA layer both reused it next): fixed-256-slot,
pure-atomic, lock-free `(kind, key bytes<=108, owner pid)` side-index. `Pipes.litebox`'s stale
pointer (fixed via `GlobalStateHandle::pipes()`) and `FutexManager`'s stack-allocated `LoanList`
sharing hang (fixed: each process gets its own fresh `FutexManager`) were two more instances of
the same raw-pointer-frozen-into-shared-bytes root pattern. `SafeZoneAllocator::alloc`'s spinlock
livelock (distinct mechanism, no dead-holder recovery unlike `RawMutex`) is still open.

## Closed — do not re-attempt without a genuinely new approach

VEH_FRAME_STRIDE canary guard, `dev_bench`/`litebox_runner_snp` Windows build failures, CoW-mmap
performance, input-latency bugs, presenter-split duplicate-`SYN_REPORT`, the GUI-protocol
(DRM/KMS+wgpu) decision, five cheap-wins PRD rows, cross-process-fork stdio-handle bug
(`spawn_suspended`'s clobbered `STARTF_USESTDHANDLES` blocks), presenter-process split
(`litebox_presenter_protocol` + `ControlServer` zero-copy scanout, `docs/presenter-process-
design.md`) — all CLOSED, none open. Full detail: archive.

## Docs and tooling map

- **Archives** (newest first) — `_2026-09-18.md` (12th-19th passes: Xvfb/dbus relaxed; shared
  AF_UNIX connection plane; isolated repro PASSED; `wait_on_tun` REFUTED + smoltcp-panic FIXED;
  `-S`/`-e` script bug FIXED; `xset` silent kill CAUGHT LIVE + FIXED; GlobalState field audit +
  debug binary + unambiguous stall read; live cdb repro of the ppoll stall twice, has_pending
  REFUTED, non-blocking-connect `Init`-state gap found), `_2026-09-17.md` (shell-crash investigation, stdio-handle
  bug, 12 registry/pointer/lock fixes, writable-layer-race fix), `_2026-09-16.md` (popup-menu
  re-test, Track A audit, RawMutex/presenter), `_2026-09-15.md` (ACK-stall-kill), `_2026-09-10.md`
  (fork fd eligibility, OCI cache, s6-boot, browser config, crash-dump/VEH, CoW). Older:
  `_2026-09-03.md`, `_2026-09-05.md`.
- Fork: `docs/track-b-fork-fix-progress.md`, `advisor/ADVISORY-002-d-zero-fork.md`,
  `advisor/ADVISORY-001-fundamentals.md` (§3N tcache). `docs/veh-exception-handler-design.md` —
  read before touching the VEH handler.
- Desktop logs: `docs/webtop-debian-selkies-2026-09-06.md`, `webtop-alpine-mate-2026-09-07.md`,
  `webtop-debian-xfce-2026-09-08.md`, `webtop-xfce-code-vs-data-2026-09-08.md`, `fork-fs-veh-2026-09-08.md`.
- Consult before deriving: `docs/premade-library-research.md`, `docs/drm-dumb-buffer-ioctl-reference.md`,
  `docs/diag-timeline-field-semantics.md` (before any `DIAG_TIMELINE` `comm`-field hypothesis).
- `docs/macos.md` — Apple Silicon guest-execution context switch is a stub, deferred. Designs NOT
  implemented: `docs/session-daemon-design.md`, `docs/fork-region-grouping-design.md`.
- `advisor/probes/` — diagnostics (`decode_frame.py`, `symbolize_litebox_crash.py`, `dup_probe.c`,
  `drm_flip_probe.c`, `clone_probe.c`, `socketpair_fork_probe.c`) plus `MEASUREMENT-PITFALLS.md`,
  `DISK-HYGIENE.md`.
- `.gm/memories/` — older per-topic notes, superseded by this file/archives wherever they overlap.
