# litebox — current state (2026-09-20)

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

**Passes 4-17 (2026-09-17/18), all FIXED and live-verified — full narrative: `docs/
AGENTS_ARCHIVE_2026-09-17.md`/`_2026-09-18.md`.** (4) nginx-self-test pipe-EOF wedge, fixed via
`PROC_THREAD_ATTRIBUTE_HANDLE_LIST` explicit handle allow-list. (8) `Network::socket_set` made
shared-arena-native (256-slot fixed array). (10) post-`NGINX_STARTED` CPU livelock —
`LocalPortAllocator`/`closing_in_background` converted to fixed `[u16; 65535]` arrays
(`interface`/`queued_for_closure` remain open). (11) `RawMutex` poison-on-dead-holder scheme;
`Socket::Drop` cross-process-heap-free fixed via `core::mem::forget`. (12) by-name `Xvfb`/
`dbus-daemon` cross-process-fork exclusion relaxed (`SafeZoneAllocator::dealloc`'s spinlock
livelock found, **still open**). (13) shared cross-process AF_UNIX connection data plane DESIGNED
+ IMPLEMENTED (`SharedUnixConnTable`/`SharedUnixConnectQueue`). (14/15) isolated AF_UNIX repro
PASSED; real full-boot-stall root cause was a smoltcp stale-`SocketHandle` panic killing
`net_worker` threads platform-wide, FIXED via `catch_unwind` + `force_reset_network_after_panic()`
(the `wait_on_tun` two-holder-deadlock theory was WRONG — symbol-resolution noise, trust only small
offsets). (16) `webtop_stack.sh`'s `[ -S "$XSOCK" ]` never true on this shim, fixed: `-S` → `-e`.
(17) `xset q`'s silent kill CAUGHT LIVE + FIXED (`memfds`/`shared_files` per-process-shadowed).

**Eighteenth pass** — systematic `GlobalState` field audit (3 more defects fixed: `unix_addr_
table`/`fifo_registry` per-process-shadowed, `sysv_shm` fixed 128-slot array), a debug build for
reliable `cdb` symbols. Still open, deeper redesign needed: `pty_registry`/`daemon_pty_masters`/
`flock_registry`/`drm`/`evdev` (pickup list). Found the real blocked thread: guest `ppoll()`
parked on a Condvar, AF_UNIX bounded-15ms-repoll running but the awaited `has_pending(...)` never
flips true. Full detail: archive.

**Nineteenth pass** — live `cdb -pv` repro of the ppoll stall, `has_pending`/queue logic REFUTED
as the bug; found a real-but-unconfirmed `Init`-state gap in `UnixInitStream::check_io_events`.
Full detail: archive.

**Twentieth pass** — FIXED the `SharedUnixConnTable` slot leak on externally killed clients
(commit `05d279d`; dead-holder check + reclaim, `SHARED_UNIX_CONN_CAPACITY` 8->64). Confirmed the
AF_UNIX bounded-15ms-repoll IS engaged and re-scanning -- not the bug. Found (not yet fixed) a
second `wait4(-1)` stall with no bounded-repoll fallback. Full detail: archive.

**Twenty-first pass** — FIXED the `wait4(-1)` stall (commit `a771692`): no bounded-repoll
fallback on `pid == -1`. **Twenty-second/twenty-third passes** — `_nofork_tick()` sleep-forks-
every-iteration fix (script-only) + its own dash-`$SECONDS` regression fix; AF_UNIX rendezvous
"livelock" theory raised then REFUTED by the twenty-fourth pass. **Twenty-fourth pass** — live
per-request instrumentation proved the AF_UNIX rendezvous mechanism itself sound (one real connect
per boot, ~23ms, no timing/address-mismatch); real blocker found upstream (`$XSOCK` wait loop never
completing); also fixed `sys_wait4`'s `pid > 0` no-repoll gap. Browser milestone not reached by any
of these three passes. Full detail (all three): archive.

Both boots killed cleanly (WMI `Terminate`), RAM ranged 1.4-4.8GB free, recovered fully after each
kill.

**Twenty-fifth pass** — CONFIRMED (direct source read of `CONTAINER_FS_SNAPSHOT_ENV_VAR`'s own doc
comment) that a bound AF_UNIX path needs only presence, not content, sync: FIXED via the already-
shared `SharedUnixAddrPresenceTable` consulted on `ENOENT` (`litebox::fs::devices::
cross_process_bound_unix_socket_status`, `do_stat`/`do_access` in `litebox_shim_linux/src/
syscalls/file.rs`) — closed the `$XSOCK` stall for good. Also widened
`SHARED_UNIX_CROSS_CONNECT_TIMEOUT` 3s->15s (real contention starved the listener's 15ms re-poll).
Superseded by the twenty-sixth pass's deeper fix below (the "established connection's data
doesn't flow" finding this pass ended on). Full evidence: archive.

**Twenty-sixth pass (2026-09-20) — ROOT-CAUSED AND FIXED the "Xvfb never reads `xset q`'s bytes"
gap; `XVFB_UP` printed for the first time ever.** Root cause: `EpollFile::
repoll_stdin_and_timerfd_interests` decided ready-set membership from `EpollEntry::poll`'s
`is_still_ready` field instead of its `event.is_some()` field — `is_still_ready` is
unconditionally `false` for any `EPOLLET`-registered fd, and Xvfb registers its accepted X11
client fd exactly that way. Fixed (`litebox_shim_linux/src/syscalls/epoll.rs`), live-verified on
both debug and release binaries. Boot reached `[s] DE_LAUNCHED` and attempted `SELKIES_PORT_UP`
(curl_exit=137 — safety-killed at host RAM ~320-480MB, debug-binary-only, not a further litebox
bug). Full evidence: `docs/AGENTS_ARCHIVE_2026-09-18.md`, twenty-sixth pass.

**Twenty-seventh pass (2026-09-20) — surgical pre-create LANDED (safe); general periodic-sync
mechanism attempted, PROVEN to cause a worse regression than it fixed, cleanly reverted.**
(1) **Surgical fix, kept**: `/tmp/empty`, `config/` and `config/.Xresources` (deterministic,
never-guarded-by-a-`[ -d ]`-check content) now ship baked into `.wfgy/webtop_seed.tar` itself
(imported via `--resume-from` before any fork happens), so every process in the boot tree has them
from time zero — no visibility race to lose. `usr/share/selkies/web/50x.html` was deliberately
**not** pre-seeded: `webtop_stack.sh:127` guards a REAL one-time dashboard-directory copy behind
`[ ! -d /usr/share/selkies/web ]`, and pre-creating that directory would silently skip the real
copy for every process, replacing the dashboard with just the error page. Live-verified: no
regression, byte-identical boot progress to before this change.
(2) **General fix, tried and reverted**: a background thread per real OS process (`run()` and the
real cross-process fork-child bootstrap `diag_process_fork_task_resume_probe` — confirmed real via
`[process_fork_diag] task-resume-probe` lines despite the `diag_` naming) periodically exported+
published this process's writable layer to the canonical snapshot and additively merged missing
entries back (AF_UNIX-bounded-repoll-shaped). It DID fix the target problem (`dbus-daemon`'s
`/tmp/addr` write became visible to the parent shell's poll) but caused a WORSE, 100%-reproducible
regression: `webtop_stack.sh`'s `cp /defaults/default.conf` + four sequential `sed -i` calls (each
its own real fork+`wait4`) raced the periodic export, capturing a self-consistent but TEMPORALLY
STALE snapshot (post-`cp`, pre-`sed`); `publish_as_container_fs_snapshot`'s existing size-based
"bigger wins" tie-breaker (never a timestamp) then let that stale snapshot PERMANENTLY clobber the
shell's fresher, correct one. Result: `NGINX_SUPERVISOR: giving up after 30 attempts` every time,
literal unsubstituted `invalid port in upstream "127.0.0.1:CWS"` — in all 3 tested variants (500ms;
3000ms; 3000ms + "skip if canonical modified <750ms ago"), 100% reproduction, NOT reduced by
widening the interval — the tell that this is not a rare coincidence: N full-tree-walk threads'
aggregate CPU/I/O cost slows the exact fork+wait4+import sequences the fix depends on completing
fast, so widening the interval only grows the contention that recreates the same collision at the
new timescale. CONFIRMED via isolation (`LITEBOX_NO_WRITABLE_LAYER_SYNC=1`, surgical fix kept): 0
CWS occurrences, clean `NGINX_STARTED`, boot proceeds to `XVFB_UP`/`DBUS_FAILED` exactly as before.
Mechanism (`spawn_writable_layer_sync_thread`/`merge_missing_writable_layer_entries`,
`litebox_runner_linux_on_windows_userland/src/lib.rs`) fully REMOVED, not disabled — do not re-add
a per-process timer-driven full-tree export/publish without first fixing one of pickup item 3's
two structural prerequisites, verified against this same nginx repro. Did not reach the browser/
terminal/apps milestone this pass. RAM 1.8-6.5GB free throughout; every process cleanly WMI-
`Terminate`d before this pass ended.

### Track B — current pickup list, precise (full pass-by-pass evidence: archive)

(-1) ~~Build the minimal isolated cross-process AF_UNIX repro~~ — DONE, fourteenth pass. (0)

(0b) `SafeZoneAllocator`'s `spin::mutex::SpinMutex` (`litebox/src/mm/allocator.rs`) needs the same
dead-holder-recovery treatment `RawMutex` already has — live-caught spinning forever in `dealloc`,
high blast radius, own dedicated pass. (1b) `queued_for_closure`'s own still-open cross-process-Vec
hazard (STORAGE not yet converted to a fixed pointer-free array the way `closing_in_background`/
`socket_set` already were) remains a live risk. (2) debugger-root-cause `litebox/src/
event/wait.rs:224`'s `unreachable!()` on garbage thread state (dozens per boot, most frequent
panic historically, NOT yet debugger-confirmed — do not patch blind); (3) **still the sole
confirmed blocker to the browser/terminal/apps milestone**: `DBUS_FAILED` — `dbus-daemon --nofork`
writes `/tmp/addr` but never forks/exits again, so the parent shell's `_nofork_tick`-based poll
(a pure busy-wait, zero forks, confirmed by direct read of `webtop_stack.sh`'s own `_nofork_tick`)
never gets a synchronization point to see it. A periodic full-tree timer-based sync is DISPROVEN
(twenty-seventh pass, above) as a viable general fix — it is fundamentally incompatible with
`publish_as_container_fs_snapshot`'s existing size-based tie-breaker once a high-frequency extra
writer is added. The real fix needs ONE of: (a) replace that tie-breaker with a real
timestamp/generation counter so a stale publish can never beat a fresher one regardless of size or
publish frequency; (b) a lock that any multi-entry import (`import_all`/
`import_cross_process_writable_layer`) holds as writer and any export/walk holds as reader, so an
export can never observe a filesystem mid multi-file-import; (c) extend the
`SharedUnixAddrPresenceTable` POD/lock-free flat-table pattern (already proven for AF_UNIX
presence, `sysv_shm`) to small file CONTENT specifically for long-lived, non-forking daemons'
LATE writes — narrower in scope than a general sync, but sidesteps both (a) and (b) entirely.
Whichever is chosen, verify with the SAME nginx `cp`+multi-`sed` sequence as a regression probe
before declaring victory. (4) finish the `Network` shared-arena
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
0:glibc.malloc.mxfast=0` as an `--env` runner flag (a bare host-shell prefix does NOT reach the guest).
Glibc-only workaround, not a fix (PRD `glibc-tunables-workaround-pending-zero-fork`);
`LITEBOX_PROCESS_FORK=1` removes the class properly but can't run a desktop until the AF_UNIX gap
below closes. Selkies also needs `--clipboard-enabled=false` (its clipboard monitor re-triggers the
same corruption every tick).

**Sixth/seventh pass (closed)** — writable-layer-adoption race fixed via the existing
atomic-rename primitive; live-verified 5/5 boots, zero recurrence. Detail: archive.

**Open here.** One client per selkies instance, no slot reclaim on reload. Architectural gap:
**guest processes share no AF_UNIX/loopback/FIFO namespace** — precisely confirmed and named
(`unix_addr_table`'s `Backlog`/`Channel` connection data) in the twelfth-pass entry above; extending
that table's presence-sharing pattern to real connection data (now DONE, thirteenth pass, and
proven sound in isolation by the fourteenth-pass repro) was meant to put the desktop on the
crash-free path — the fourteenth-pass full-boot stall is the thing standing between here and there.

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

VEH_FRAME_STRIDE canary guard, `dev_bench`/`litebox_runner_snp` build failures, CoW-mmap
performance, input-latency bugs, presenter-split duplicate-`SYN_REPORT`, the GUI-protocol
decision, five cheap-wins PRD rows, cross-process-fork stdio-handle bug (`spawn_suspended`'s
clobbered `STARTF_USESTDHANDLES`), presenter-process split (`docs/presenter-process-design.md`)
— all CLOSED, none open. Full detail: archive.

## Docs and tooling map

- **Archives** (newest first) — `_2026-09-18.md` (12th-22nd passes: Xvfb/dbus relaxed; shared
  AF_UNIX connection plane; isolated repro PASSED; `wait_on_tun` REFUTED + smoltcp-panic FIXED;
  `xset` silent kill FIXED; GlobalState field audit + debug binary; `SharedUnixConnTable` leak +
  `wait4(-1)` no-repoll stall both ROOT-CAUSED + FIXED; sleep-fork-per-iteration FIXED, real
  `ppoll` blocker found; browser milestone still not reached), `_2026-09-17.md` (shell-crash investigation, stdio-handle
  bug, 12 registry/pointer/lock fixes, writable-layer-race fix), `_2026-09-16.md` (popup-menu
  re-test, Track A audit, RawMutex/presenter), `_2026-09-15.md` (ACK-stall-kill), `_2026-09-10.md`
  (fork fd eligibility, OCI cache, s6-boot, browser config, crash-dump/VEH, CoW). Older:
  `_2026-09-03.md`, `_2026-09-05.md`.
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
  `drm_flip_probe.c`, `clone_probe.c`, `socketpair_fork_probe.c`) plus `MEASUREMENT-PITFALLS.md`,
  `DISK-HYGIENE.md`.
- `.gm/memories/` — older per-topic notes, superseded by this file/archives.
