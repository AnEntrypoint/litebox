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

**Per-fork cost** was ~3.5-5s, now ~1.2s; a full `webtop_stack.sh` boot reaches `NGINX_STARTED` in
under a minute versus never in 15+. Older cost explanations were measured wrong. Use
`LITEBOX_DIAG_FORK_TIMING=1` for the next cost question. Three correctness bugs fixed; detail: archive.

**Still open**: nginx's own SSL-cert generation fails on its first real startup attempt — the original
symptom this investigation began from, genuinely not root-caused (`docs/track-b-fork-fix-progress.md:
146-152`). The TOP-LEVEL parent's curl-self-test stall (`sys_wait4(pid=-1)` not checking
`cross_process_children`) is fixed (`6e86a40`) — do not cite that one as open.

**The nginx-self-test pipe-EOF wedge (fourth pass, 2026-09-17) — CONFIRMED and FIXED.** Root
cause: broad `bInheritHandles=TRUE` leaked a sibling fork child's inheritable bridge-pipe handle
into unrelated children racing the same window. Fix: `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` explicit
per-call handle allow-list (`process_fork.rs`, `spawn_suspended_impl`). Live-verified. Archive.

**`Network::socket_set` made shared-arena-native (eighth pass)**: fixed a `tuple.unwrap()` panic via
a 256-slot (`MAX_SOCKETS`) fixed array in the shared kernel arena instead of a private-heap `Vec`.
A/B-confirmed (ninth pass): old binary crash-loops 8x on it, new binary zero. Archive.

**Post-`NGINX_STARTED` CPU livelock — ROOT-CAUSED and FIXED (tenth pass, 2026-09-17).** A thread
burning 100% of a core forever inside `LocalPortAllocator::ephemeral_port`/`deallocate`
(`hashbrown` SIMD probe on a `HashMap` whose backing table is private-per-process-heap, meaningless
to an attaching fork child). Fixed: converted to a fixed pointer-free `[u16; 65535]` array
(`litebox/src/net/local_ports.rs`); a second instance (`closing_in_background`, same mechanism)
converted the same way (`litebox/src/net/mod.rs`). `interface`/`queued_for_closure` remain open.
Verified clean build + 25 net unit tests + two live re-runs, livelock confirmed gone. Detail: `docs/
AGENTS_ARCHIVE_2026-09-17.md`.

**Poison-on-dead-holder scheme for `Network` — DESIGNED, IMPLEMENTED, LIVE-VERIFIED (eleventh
pass).** `RawMutex` gained a `poisoned: AtomicBool`; `GlobalStateHandle::net_lock`
opts in via `lock_recovering_poison()`, and on poison `Network::reset_after_poisoning`
wholesale-resets `socket_set`/`closing_in_background`/`queued_for_closure`/`local_port_allocator`
(`SocketHandle` has no generation counter, so wiping every field is the only way to tell stale
from live). Also fixed the same pass: a removed `Socket`'s normal `Drop` freed its RX/TX ring
`Vec`s through the CURRENT process's allocator on a pointer naming a dead process's private heap —
fixed via `core::mem::forget` (that memory was never this process's to free). Five live
`LITEBOX_PROCESS_FORK=1` boots: real dead-holder-recovery fired live, zero panic-cascade; furthest
reached `SELKIES_PORT_UP`+`DE_LAUNCHED` with one disclosed non-fatal residual panic. Did NOT reach
the browser/terminal/apps milestone — attributed then to the Xvfb/dbus by-name exclusion, now
superseded by the twelfth-pass entry below. Detail: `docs/AGENTS_ARCHIVE_2026-09-17.md`.

**Twelfth pass**: by-name `Xvfb`/`dbus-daemon` cross-process-fork exclusion relaxed — both now
cross-process-fork for real (log-proven). `XVFB_FAILED`/`DBUS_FAILED` still fire, root cause
precisely named: `unix_addr_table`'s `Backlog`/`Channel` connection DATA is still per-process-heap
(presence side-index already shared) — attempted by the thirteenth pass below, still not fully
closed (fifteenth pass). Same pass, separately: `SafeZoneAllocator::dealloc`'s
`spin::mutex::SpinMutex` (`litebox/src/mm/allocator.rs`) live-caught spinning forever (no
dead-holder recovery, unlike `RawMutex`) — **still open**. Full narrative: archive.

**Fork-after-Xorg PERMANENT freeze — did NOT reproduce 2026-09-17; thread-based-fork-only.** Under
`LITEBOX_PROCESS_FORK=1` the identical script completed cleanly 2/2 — zero freeze, zero double-free.
Full evidence, a disclosed ENOMEM finding under concurrent cross-process forks: archive.

**Thirteenth pass, 2026-09-18 — shared cross-process AF_UNIX connection data plane DESIGNED and
IMPLEMENTED** (`SharedUnixConnTable`/`SharedUnixConnectQueue`, `syscalls/unix.rs`'s module doc has
the full design), three real bugs found+fixed along the way (stack overflow on a by-value fixed
array, an ambiguous-`None`-timeout infinite-poll bug, a missing shared-queue check in event-driven
`accept()` paths). Full narrative: archive.

**Fourteenth pass — isolated AF_UNIX repro PASSED clean; full-boot stall theory since REFUTED
(fifteenth pass below).** `SharedUnixConnTable`/`SharedUnixConnectQueue` genuinely work for the
minimal cross-process case (byte-exact round trip, fork-before-listen probe). This pass's own read
of the subsequent full-boot stall (`wait_on_tun`/`with_fork_duplicate_claim_owner` two-holder
deadlock) was WRONG.

**Fifteenth pass, 2026-09-18 — real root cause found and FIXED, live-verified: a smoltcp
stale-`SocketHandle` panic killed `net_worker` threads platform-wide.** `wait_on_tun` is
structurally incapable of blocking (single lock site, every caller caps its timeout to 1ms) —
the fourteenth pass's "two-holder deadlock" read was a cdb sample catching this thread's own
permanently-present idle noise, not a hang; several other frames sampled this investigation
(`prepare_for_exit+0x602f`, `pty_ioctl+0x6bbc`, huge offsets into short functions calling things
their own source can't reach) are release/LTO/ICF symbol-resolution artifacts, not literal call
stacks (matches `docs/AGENTS_ARCHIVE_2026-09-17.md:1852`'s prior note on the same phenomenon — trust
only small, plausible offsets). **Real mechanism**: `Network::reset_after_poisoning`'s own doc
comment already disclosed the gap — a still-alive process's OWN descriptor-table entry can keep
naming a `SocketHandle` the reset just wiped, and smoltcp's `SocketSet::get`/`get_mut` (0.12, no
generation counter) panics outright (`"handle does not refer to a valid socket"`). That panic was
UNCAUGHT inside `net_worker`'s loop, permanently killing that process's networking; since `Network`
is shared across the whole fork family, every OTHER process's `net_worker` died the same way in
turn until networking silently stopped everywhere — matching the observed total, permanent,
platform-wide log silence exactly. **Fix, two parts, both required** (part 1 alone, live-tested,
only turned it into an infinite catch-repanic loop): (1) `net_worker`'s two closures
(`litebox_runner_linux_on_windows_userland/src/lib.rs`) now `catch_unwind` around
`perform_network_interaction()` and call a new `LinuxShim::force_reset_network_after_panic()`
instead of propagating; (2) `litebox/src/net/mod.rs` gained `Network::socket_set_contains` (a
bounded linear scan — smoltcp has no checked `get`) guarding `remove_dead_sockets`,
`close_pending_sockets`, and `drain_socket_channel_buffers`'s TCP/UDP/listener-handle paths against
ever touching an already-removed handle again. Live-verified: the panic signature did not recur in
a full run with both parts. **A SECOND, different, not-yet-root-caused stall found past this fix**
(one process genuinely blocked in what looks like `sys_wait4`, per the closure-shape of its
`wait_until` instantiation, but with unreliable enclosing frame names) — did not reach
`XVFB_UP`/`DBUS_UP`/browser/terminal/apps this pass either. Full evidence, exact log lines, the
insufficient-first-fix detail, and concrete next steps: `docs/AGENTS_ARCHIVE_2026-09-18.md`.

### Track B — current pickup list, precise (full pass-by-pass evidence: archive)

(-2) ~~root-cause `net::wait_on_tun`/`with_fork_duplicate_claim_owner`~~ — REFUTED, fifteenth
pass: structurally can't deadlock (single lock, every caller caps its timeout to 1ms); the real
mechanism was a smoltcp stale-`SocketHandle` panic killing `net_worker` platform-wide, now FIXED
(above) and a third confirmation run reached a stable `HOLD` steady state (559 forks, zero panics,
zero permanent stall) with no intervention needed — a SECOND stall this pass's own second run hit at
a similar point did NOT reproduce a third time, consistent with this whole area being genuinely
probabilistic rather than a deterministic bug; not re-prioritized unless it recurs. **The browser
milestone is still blocked by the ALREADY-TRACKED, pre-existing gap named in "Open here" below**
(`unix_addr_table`'s connection-DATA sharing for Xvfb's own X11 socket specifically — confirmed live,
third run: `xset`'s connect got the exact `ECONNREFUSED ... DIFFERENT guest pid` WARN, then the
script's own 60s timeout killed both `Xvfb` and `xset` before the shared-queue mechanism resolved
it, despite that same mechanism passing the fourteenth pass's minimal isolated repro) — NOT a new
regression from this pass. Full evidence: `docs/AGENTS_ARCHIVE_2026-09-18.md`, fifteenth pass.

(-1) ~~Build the minimal isolated cross-process AF_UNIX repro~~ — DONE, fourteenth pass. (0)

(0) **~~TOP PRIORITY, twelfth pass~~ — superseded by thirteenth-pass entry above.** (0b) `SafeZoneAllocator`'s `spin::mutex::SpinMutex` (`litebox/src/mm/allocator.rs`)
needs the same dead-holder-recovery treatment `RawMutex` already has — live-caught spinning forever
in `dealloc` this pass, high blast radius, own dedicated pass. (1) ~~Debugger-root-cause the
dead-holder-recovery data-inconsistency panic~~ — DONE, eleventh pass. (1b) `queued_for_closure`'s
own still-open cross-process-Vec hazard (nothing yet converts its STORAGE to a fixed pointer-free
array the way `closing_in_background`/`socket_set` already were) remains a live risk: any process
reading/pushing it while attached rather than constructing could still hit the stale-pointer class
on the Vec header itself; (2) debugger-root-cause `litebox/src/event/wait.rs:224`'s
`unreachable!()` on garbage thread state (dozens per boot, most frequent panic historically, NOT
yet debugger-confirmed — do not patch blind); (3) root-cause the `/tmp/empty` writable-layer
cross-child-visibility gap; (4) finish the `Network` shared-arena redesign (`interface`,
`queued_for_closure` remain); (5) after (0)-(4), `timerfd`/`signalfd` are the next-cheapest
carriable fd kinds before attempting `socket`/`unix-socket`/`pty`/`epoll`.

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

**Sixth/seventh pass (closed)** — a writable-layer-adoption race (shared snapshot-path deleted by
the first of several importers, non-atomic export let a concurrent importer read a torn tar) fixed
via the existing atomic-rename primitive; live-verified 5/5 boots, zero recurrence. Detail: archive.

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

## Cross-process-capable `RawMutex` -- done, live-verified (detail: `docs/AGENTS_ARCHIVE_2026-09-16.md`)

`RawMutex` no longer calls `WaitOnAddress`/`WakeByAddressSingle` (process-local per MSDN) -- manual
wait queue + one auto-reset kernel `Event` per OS thread, cross-process half real
(`DuplicateHandle`-based). Live-verified under real multithreaded contention, no hang/deadlock.

## Shared kernel heap -- selective-routing correction landed (detail: `docs/AGENTS_ARCHIVE_2026-09-17.md`)

`SLAB_ALLOC` stayed on the private per-process `VirtualAlloc2` path (routing everything through one
shared bump allocator exhausted an 8 GiB pool in 45-90 execs); the fixed-base/handle-inherit
machinery instead backs a small **64 MiB, standalone, bounded** `shared_kernel_arena_alloc`
(`lib.rs`), NOT wired to `GlobalAlloc`, reserved for `LiteBoxX`/`GlobalState` placement only.

### `SharedArc<T>` and real `GlobalState` create-vs-attach -- DONE 2026-09-17; does NOT close XVFB_FAILED/DBUS_FAILED (detail: archive)

Hand-rolled `SharedArc<T>` places `value`+`strong: AtomicUsize` in the 64 MiB arena;
`SharedKernelStateProvider` gives `LinuxShimBuilder::build` a real attach-or-create branch.
**Root cause precisely characterized**: `SharedArc::new` shares only `T`'s literal inline bytes --
every `GlobalState` REGISTRY (`unix_addr_table`, `pty_registry`, `daemon_pty_masters`,
`flock_registry`, `fifo_registry`, `sysv_shm`, `memfds`, `shared_files`) is a `BTreeMap`/similar
whose NODES live on the private per-process heap, meaningless to an attaching process. PRD:
`globalstate-nested-collections-not-actually-shared`.

## `unix_addr_table` presence sharing -- landed 2026-09-17 (real blocker was elsewhere; detail: archive)

`SharedUnixAddrPresenceTable` (`syscalls/unix.rs`): fixed-256-slot, pure-atomic, lock-free
`(kind, key bytes<=108, owner pid)` side-index mirrored alongside each process's real
`unix_addr_table` `BTreeMap`. **The reusable flat-table PATTERN** the connection-DATA layer
(below) and the other still-real registries (`pty_registry` et al.) needed next.

## Cross-process fork: twelve registry/pointer/lock fixes, landed 2026-09-17 (detail: archive)

Root pattern (11 of 12): a raw `Arc`/`Box` pointer frozen into cross-process-shared bytes by
whichever process constructs `GlobalState` first, dangling in every attaching process -- fixed by
per-process-copy shadowing, a locking accessor (`net_lock`, `pipes()`), or a fixed-slot
pointer-free array. Instance 12 (`net_lock` left locked by an exiting fork-child) was a
lock-liveness gap, fixed via `RawMutex` owner-death recovery (below).

## RawMutex lost-wakeup, Pipes stale-pointer, FutexManager sharing gap, lock-orphaning -- ALL FOUR FIXED 2026-09-17 (detail: archive)

(A) `RawMutex::resolve_waiter_event`'s cross-process branch panicked on a stale pid -- `waiters` is
now a fixed-32-slot pointer-free `WaiterQueue`. (B) `Pipes.litebox`'s stale pointer crashed a
killed fork child's stdio teardown -- rebound via `GlobalStateHandle::pipes()`. (C) `FutexManager`
cross-process sharing hung on stack-allocated `LoanList` entries -- each process gets its own fresh
`FutexManager`. (D) A cross-process-fork child's un-shutdown `net_worker` could be killed
mid-hold of `net_lock` -- fixed with `RawMutex` owner-death recovery. `SafeZoneAllocator::alloc`'s
spinlock livelock (distinct from D, still open) was live-caught in the twelfth pass, above.

## Closed — do not re-attempt without a genuinely new approach

VEH_FRAME_STRIDE canary guard, `dev_bench`/`litebox_runner_snp` Windows build failures, CoW-mmap
performance, input-latency bugs, presenter-split duplicate-`SYN_REPORT`, the GUI-protocol
(DRM/KMS+wgpu) decision, five cheap-wins PRD rows, cross-process-fork stdio-handle bug
(`spawn_suspended`'s clobbered `STARTF_USESTDHANDLES` blocks), presenter-process split
(`litebox_presenter_protocol` + `ControlServer` zero-copy scanout, `docs/presenter-process-
design.md`) — all CLOSED, none open. Full detail: archive.

## Docs and tooling map

- **Archives** (newest first) — `_2026-09-18.md` (12th pass: Xvfb/dbus relaxed, SafeZoneAllocator
  livelock; 13th pass: shared AF_UNIX connection data plane; 14th pass: isolated repro PASSED; 15th
  pass: `wait_on_tun` stall theory REFUTED, real smoltcp-panic root cause FIXED, second stall found),
  `_2026-09-17.md`
  (shell-crash investigation, stdio-handle bug, 12 registry/pointer/lock fixes, writable-layer-race
  fix), `_2026-09-16.md` (popup-menu re-test, Track A audit, RawMutex/presenter), `_2026-09-15.md`
  (ACK-stall-kill), `_2026-09-10.md` (fork fd eligibility, OCI cache, s6-boot, browser config,
  crash-dump/VEH, CoW). Older: `_2026-09-03.md`, `_2026-09-05.md`.
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
