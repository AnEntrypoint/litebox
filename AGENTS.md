# litebox — current state (2026-09-17)

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

**Eligibility** — refused only for `comm` == `Xvfb`/`dbus-daemon` (a live unix listening socket can't be
served from a fork-time filesystem snapshot), an already-borrowed fd table, a beyond-stdio fd that isn't
a pipe end/path-recorded regular file/eventfd/close-on-exec (overridable by
`LITEBOX_PROCESS_FORK_IGNORE_FDS`), or an unsanitizable `fs_base`/context. On a real `debian-xfce` boot
the only remaining blocking kind is `unix-socket` — 5 refused forks of 34, down from 34/34. Per-kind
deviations: archive.

**Per-fork cost** was ~3.5-5s, now ~1.2s; a full `webtop_stack.sh` boot reaches `NGINX_STARTED` in
under a minute versus never in 15+. Older cost explanations were measured wrong. Use
`LITEBOX_DIAG_FORK_TIMING=1` for the next cost question. Three correctness bugs fixed; detail: archive.

**Reading a cross-process log** — the `fork_verify` "stale CODE pointer" noise-vs-signal read: archive.

**Still open**: nginx's own SSL-cert generation fails on its first real startup attempt — the original
symptom this investigation began from, genuinely not root-caused (`docs/track-b-fork-fix-progress.md:
146-152`). The TOP-LEVEL parent's curl-self-test stall (`sys_wait4(pid=-1)` not checking
`cross_process_children`) is fixed (`6e86a40`) — do not cite that one as open.

**NEW, found live 2026-09-17, full `.wfgy/webtop_stack.sh` boot under `LITEBOX_PROCESS_FORK=1` (a
new best: reliably reaches `NGINX_STARTED`, past the shared-kernel-heap commit-exhaustion wall
which is now separately fixed), NOT fixed: a nested variant of the already-fixed curl-self-test
stall.** Top-level pid 1 blocks reading the `$(curl ...)` self-test's pipe forever; the writer (a
cross-process-fork child that already finished its own script-visible work) is itself stuck one
level deeper, inside its own `prepare_for_exit()` reaping ITS OWN (thread-based-forked) child,
blocked in `RawMutex::block` forever. `6e86a40`'s fix only covers the TOP-LEVEL parent's own
`wait4`; this is a cross-process child's OWN nested wait4, a locus that fix never touched. Not
root-caused (two live candidate leads, undistinguished); `XVFB_UP`/`DBUS_UP`/`DE_UP`/browser were
NOT reached this session as a direct result. Full mechanism, cdb evidence, and a process-hygiene
lesson (`cdb -p <pid>` alone is INVASIVE — a bare `q` KILLS the debuggee; always use `-pv`/`qd`):
archive.

**Fork-after-Xorg PERMANENT freeze — did NOT reproduce 2026-09-17; thread-based-fork-only.** Under
`LITEBOX_PROCESS_FORK=1` the identical script completed cleanly 2/2 — zero freeze, zero double-free.
Full evidence, a disclosed ENOMEM finding under concurrent cross-process forks: archive.

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

**A trampoline-extension failure used to poison a whole segment's syscalls, now fixed** (archived).
**Tags, verified live, never from the name** (archived): `linuxserver/webtop:alpine-mate` ships MATE
not XFCE; `alpine-xfce` doesn't exist; `debian-xfce`/`ubuntu-xfce` ship real XFCE.

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

**The glibc/tcache crash class still sporadically hits selkies** on the THREAD-based fork path,
separately from the ACK-stall-kill (a DPI-fork on rapid reconnect SIGSEGVs, ADVISORY-001 §3N) --
**a SECOND, different corruption signature under heavy fork load** (`double free or corruption
(out)` SIGABRT), hitting bin paths `GLIBC_TUNABLES` deliberately leaves enabled. **Track B
territory, not a tunable-coverage gap** — do not re-attempt a `GLIBC_TUNABLES`/env fix without
evidence of a THIRD mechanism. Full evidence: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

### The ACK-stall-kill and port-8081 watchdog — both CLOSED (2026-09-16)

Real blocker was the guest-side patcher silently crashing on `shutil.copy2()`'s `copystat()`
(no `listxattr` shim) before ever patching `selkies.py`; fixed (`478e640`), live-verified 60+s
zero `keepalive ping timeout`. Port-8081 double-bind fix live-verified over 17 boot cycles + a
6000-connection stress test, zero recurrence, zero RAM leaks. Full detail: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

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

## Cross-process-capable `RawMutex` -- done, live-verified (detail: archive)

`RawMutex` no longer calls `WaitOnAddress`/`WakeByAddressSingle` (process-local per MSDN) --
replaced with a manual wait queue plus one auto-reset kernel `Event` per OS thread; cross-process
half is real code (`DuplicateHandle`-based), now genuinely live and fixed once (see "RawMutex
lost-wakeup" below -- the wait-queue storage itself had to become pointer-free once cross-process
contention became real). Live-verified: `yes hello | head -c 5000000 | wc -c`, `sort --parallel=4` multithreaded
contention, both exact/correct, no hang/deadlock. `process-fork-pipe-relay-sigpipe-above-4kb`
(3-stage pipeline SIGPIPE) resolved 2026-09-16, don't rely on `LITEBOX_PROCESS_FORK=1` for
heavy-iteration guests. Full internals: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

## Shared kernel heap -- selective-routing correction landed (ADVISORY-002 §3.3)

**The "route everything through one shared section" design is REVERTED.** `SLAB_ALLOC`
(`#[global_allocator]`, `lib.rs`) is back to the private per-process `VirtualAlloc2` mechanism for
every ordinary host-heap allocation -- routing everything through the shared bump allocator (no
reclaim) was live-proven to exhaust an 8 GiB pool after 45-90 real execs, worse than not sharing at
all; **live-verified fixed**. The fixed-base/atomic-cursor/handle-inherit machinery is NOT deleted
-- it now backs a small **64 MiB, standalone, bounded**
arena (`shared_kernel_arena_alloc`, `lib.rs`), deliberately NOT wired to `GlobalAlloc`, reserved for
`LiteBoxX`/`GlobalState`-only placement. Full mechanism/history: `docs/AGENTS_ARCHIVE_2026-09-17.md`.

### `SharedArc<T>` and real `GlobalState` create-vs-attach -- BOTH DONE and LIVE-VERIFIED 2026-09-17; does NOT close XVFB_FAILED/DBUS_FAILED

`litebox_platform_windows_userland/src/lib.rs`'s `SharedArc<T>` (hand-rolled, not `std::sync::Arc`,
whose `ArcInner` layout is a private std detail unsound to place by hand; stable Rust also has no
`allocator_api`) places `value` plus a `strong: AtomicUsize` in the bounded 64 MiB
`shared_kernel_arena_alloc` region; `new` -> `(handle, arena_offset)`, `attach(offset)` gets an
independent handle to the SAME bytes. `Drop` never reclaims/runs `T`'s destructor (bump allocator,
no free list; a kernel singleton must outlive the whole fork family). `SharedKernelStateProvider`
(`SharedKernelStateSlot::{LiteBoxX,ShimGlobalState}`) turns this into a real create-or-attach
protocol: trivial `Arc::new` default everywhere with real OS process isolation, real impl on
`WindowsUserland`. `litebox_shim_linux::GlobalState` is now `GlobalStateHandle<Platform, FS>` =
`Platform::Handle<GlobalStateX<...>>`; `LinuxShimBuilder::build` does the real attach-or-create
branch. **Decisive live proof**: parent bumps `next_thread_id` by a sentinel delta both before AND
after `spawn_cross_process_fork_child` returns; the child's own post-`build()` read observes both
bumps -- only possible if it is the SAME live allocation, not a snapshot or an independent copy.

**Does NOT close `XVFB_FAILED`/`DBUS_FAILED`: root cause precisely characterized.** `SharedArc::new`
places only `T`'s literal inline bytes in the arena -- but every `GlobalState` REGISTRY
(`unix_addr_table`, `pty_registry`, `daemon_pty_masters`, `flock_registry`, `fifo_registry`,
`sysv_shm`, `memfds`, `shared_files`, plus originally 3 caches -- now down to 0, see below) is a
`BTreeMap`/similar whose NODES live on the ordinary private per-process heap -- an attaching
process's copy of the root pointer is meaningless in its own address space. Follow-up PRD:
`globalstate-nested-collections-not-actually-shared`. Full mechanism: `docs/AGENTS_ARCHIVE_2026-09-17.md`.

## `unix_addr_table` presence sharing -- landed and live-verified (real blocker was elsewhere)

`litebox_shim_linux/src/syscalls/unix.rs`'s `SharedUnixAddrPresenceTable`: fixed-256-slot, pure-
`core::sync::atomic` (zero `unsafe`, zero `RawMutex`), lock-free side-index recording `(kind, key
bytes <=108, owner guest pid)` per bind/listen, mirrored alongside (never replacing) each
process's real `unix_addr_table` `BTreeMap`. A plain `GlobalState` field (`unix_addr_presence`, no
new `SharedKernelStateProvider` slot needed): zero pointer indirection, inherits whatever sharing
`GlobalState` itself already has for free. Wired at all 4 call sites (stream `listen`/`Drop`,
datagram `bind`/`Drop`) plus an always-on diagnostic on every real `ECONNREFUSED`. **This is the
reusable flat-table PATTERN the still-genuinely-shared registries below (`pty_registry` et al.)
need next** — `unix_addr_table`'s own full `BTreeMap` (the `Backlog`/`Channel` connection data,
not just presence) remains real per-process-heap and unconverted, same as those others.
Decisive live proof: parent registers one key immediately before `spawn_cross_process_fork_child`,
a second strictly after; child observes both right after its own `build()` -- proves genuine live
sharing, not a snapshot.

## Cross-process fork: twelve registry/pointer/lock fixes, all now landed, 2026-09-17

Root pattern (instances 1-11): a raw `Arc`/`Box` pointer captured once by whichever process
constructs `GlobalState` first, frozen into cross-process-shared bytes, meaningless (or dangling) in
every other attaching process -- found and fixed one layer deeper each time, isolated with a minimal
`-Z --oci-image debian:stable-slim -- /bin/bash -c 'mkdir ...'` repro under `LITEBOX_PROCESS_FORK=1`.
Fix pattern: shadow the field on `GlobalStateHandle` with a fresh per-process copy (state that
doesn't need cross-process visibility -- `litebox`, `proc_self_info`/`pts_registry`,
`elf_patch_cache`/`exec_ranges_cache`/`segment_scan_cache`, `futex_manager`), or rebind it in place
via a locking accessor (state that IS genuinely meant to be shared -- `Network`'s two fields via
`net_lock`, `Pipes.litebox` via `pipes()`), or (`RawMutex.waiters`, instance 9) replace a
process-private-heap `Vec` with a fixed-slot pointer-free array. Instance 12 (`net_lock`'s Mutex left
permanently locked by an exiting fork-child process) was a DIFFERENT shape -- not a stale pointer, a
lock-liveness/owner-death-recovery gap -- now also FIXED; see the section immediately below.
Does NOT close `XVFB_FAILED`/`DBUS_FAILED`; `pty_registry`/`flock_registry`/etc. remain real,
still-open follow-on work. Full panic signatures, bisection transcripts, WER/symbolizer evidence and
per-fix detail: archive (`docs/AGENTS_ARCHIVE_2026-09-17.md`, newest entries at the bottom).

## RawMutex lost-wakeup, Pipes stale-pointer, FutexManager sharing gap, and cross-process-fork lock-orphaning -- ALL FOUR FIXED 2026-09-17

**A. `RawMutex::resolve_waiter_event` cross-process branch -- FIXED.** Was: `RawMutex.waiters:
Mutex<Vec<WaiterRecord>>`'s `Vec` buffer is process-private-heap, so a `RawMutex` embedded in
cross-process-shared memory (any `litebox::sync::Mutex`/`RwLock` field of `GlobalState`) let an
attaching process's `wake_many` read a bogus pid and PANIC on `OpenProcess` failure -- the real
waiter (blocked in `RawMutex::block` via `do_clone`/`with_fork_duplicate_claim_owner`, confirmed live
via `cdb -p`) was never signaled. Ninth instance of the nested-collection-on-private-heap class. Fix:
`waiters` is now `WaiterQueue`, a fixed-32-slot pointer-free array guarded by a pure spin-CAS lock
(no nested OS-backed lock); `resolve_waiter_event` returns `Option<HANDLE>` and logs+skips instead of
panicking; the handle-dedup cache moved to a process-local `static` keyed by mutex address. Live-
verified reachable and non-fatal (the "queue full" fallback engaged live, zero panics). Full mechanism
and live evidence: archive.

**B. `Pipes` stale-`litebox`-pointer -- FIXED.** Tenth instance, found live while verifying A: a
`mkdir` fork child was silently `Killed`, WER showed a real `0xc0000005`, `llvm-symbolizer` resolved
it to `Vec<Option<IndividualEntry<WindowsUserland>>>::drop` -- `litebox::pipes::Pipes`'s `litebox`
field, captured once at construction (same defect `Network::rebind_per_process_fields` already fixed
twice), was stale/dangling when a killed process's inherited stdio pipe tore down. Fix: `Pipes.litebox`
is now interior-mutable (`litebox::sync::Mutex`-wrapped); `GlobalStateHandle::pipes()` rebinds before
every access, same shape as `net_lock`. Live-verified: the same repro's `d1` now exits cleanly
(`exiting with encoded status 0xc0de0000`) instead of crashing. Full mechanism: archive.

**C. `FutexManager` cross-process sharing -- FIXED (`30d4608`).** With A and B both fixed, the
identical repro still hung one step later: `FutexManager::wake` -> `LoanList::extract_if` ->
`RawMutex::block`, permanently blocked (second thread blocked the same way as A, via
`do_clone`/`with_fork_duplicate_claim_owner`). Eleventh instance at the outer level
(`FutexManager.table: Box<[LoanList<...>; N]>` is process-private-heap), but `LoanList` itself is
structurally deeper: entries are "allocated once by the caller, potentially on the stack" (its own
doc comment) -- a `FutexEntry` a guest thread registers is commonly stack-allocated, which has NO
"move it to the shared arena" fix (unlike every prior instance this session): a stack address is
fork-family-identical only for the ONE thread that actually called `fork()`, so any other thread's
stack-resident entry, or lock state a non-forking thread held at fork time, is permanently
unrecoverable garbage to every other process. Resolved the same way as `elf_patch_cache`/etc
instead: `FutexManager`'s own pre-existing doc comment already scopes it to "private"
(single-process) futexes only, so giving each process its own fresh `FutexManager` is the
already-documented intended semantics, not a workaround -- `GlobalStateHandle` carries its own
`Arc<FutexManager<Platform>>`, freshly constructed once per process in `LinuxShimBuilder::build`,
shadowing `GlobalState`'s (now removed) field. Live-verified: 10 sequential external `/bin/mkdir`
cross-process forks now complete cleanly (`OK1..OK10` + a final marker), across two independent
runs -- previously hung permanently partway through the loop.

**D. Cross-process-fork lock-orphaning hang -- FIXED.** Twelfth instance. Root cause, confirmed
live: a cross-process-fork CHILD's own `net_worker` thread (`lib.rs` ~1884) had no shutdown signal
(unlike `run()`'s own copy at ~862, despite a doc comment claiming a "verbatim" mirror) and spends a
large fraction of its life holding the one genuinely cross-process-shared `net_lock`; this child's
fast `ExitProcess` exit (skips `Drop` entirely, `main.rs`) can and does kill that thread mid-hold,
orphaning the lock permanently for the rest of the fork family -- a plain `AtomicU32` has no
OS-level "owner died" release. A graceful shutdown+join fix was tried first and was WRONG: it forces
the exiting child to contend for the shared lock against the ever-running parent's own `net_worker`,
which live-tested as a 4m45s stall ending in an external-watchdog `STATUS_FATAL_APP_EXIT` kill, worse
than the original bug -- reverted, do not retry that shape of fix. Real fix: robust-mutex owner-death
recovery on `RawMutex` itself -- new `note_locked`/`note_unlocked` trait hooks (default no-op on
every platform but Windows) record the holder's pid; `block_or_maybe_timeout` waits in bounded 2s
chunks and, only once a recorded holder is POSITIVELY CONFIRMED dead via `OpenProcess`+
`GetExitCodeProcess` (never a merely-slow-but-alive one), force-recovers the lock. Live-verified: 4
independent full 10-mkdir runs under `LITEBOX_PROCESS_FORK=1`, all exit 0 in ~7s, every run hitting
the recovery path exactly twice. Full mechanism, the reverted attempt's evidence, and file list:
archive.

Previously-recorded allocator livelock (`SafeZoneAllocator::alloc`, `SpinMutex<ZoneAllocator>`,
CLIMBING CPU) not re-investigated this pass -- still open, full detail: archive. Distinct from D
above (D's snapshots show flat CPU, no thread spinning -- a lock left locked forever, not a
livelock).

## Closed — do not re-attempt without a genuinely new approach

VEH_FRAME_STRIDE canary guard, `dev_bench`/`litebox_runner_snp` Windows build failures, CoW-mmap
performance, input-latency bugs, presenter-split duplicate-`SYN_REPORT`, the GUI-protocol
(DRM/KMS+wgpu) decision, five cheap-wins PRD rows (cargo build/fmt-verified, no boot needed) — all
CLOSED, none open. Also closed: **cross-process-fork stdio-handle bug** (`spawn_suspended`'s two
back-to-back `STARTF_USESTDHANDLES` blocks clobbered each other, no null guard on the second; PTY
test hit a separate, NOT-root-caused `signal=Signal(13)`, PRD
`cross-process-fork-pty-sigpipe-in-script-relay`) and **presenter-process split** (`litebox_
presenter_protocol` crate + runner-side `ControlServer` zero-copy scanout handoff +
`litebox-presenter.exe`, `--gui` now `Option<GuiMode>`, one real bug found+fixed: missing per-call
`OVERLAPPED`; `docs/presenter-process-design.md`). Full detail on all of the above:
`docs/AGENTS_ARCHIVE_2026-09-17.md` / `_2026-09-16.md`.

## Docs and tooling map

- **Archives** — `docs/AGENTS_ARCHIVE_2026-09-17.md` (terminal-emulator shell-crash live investigation:
  `LITEBOX_PROCESS_FORK=1` refuted as a one-line fix, cross-process-fork stdio-handle bug found;
  closed-items detail moved out of AGENTS.md), `_2026-09-16.md` (popup-menu re-test,
  `spawn_exec_collision_child` fix, Track A audit, RawMutex/presenter mechanism detail), `_2026-09-15.md`
  (ACK-stall-kill detail),
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
- `docs/macos.md` — port state; Apple Silicon guest-execution context switch is a stub, stays
  deferred (PRD `macos-aarch64-guest-execution-context-switch-is-not-implemented`).
- Designs NOT implemented: `docs/session-daemon-design.md` (`litebox_termemu`'s VT100-emulator
  slice IS implemented; the daemon/IPC layer is not), `docs/fork-region-grouping-design.md` (still
  a diagnostic probe).
- `advisor/probes/` — diagnostics (`decode_frame.py`, `symbolize_litebox_crash.py`, `dup_probe.c`,
  `drm_flip_probe.c`, `clone_probe.c`) plus `MEASUREMENT-PITFALLS.md`, `DISK-HYGIENE.md`. OCI-pull
  Python scripts there are retired.
- `.gm/memories/` holds older per-topic notes (RtlpUnwindPrologue, browser witness, XFCE/MATE/weston,
  packager OOM, image tags, cross-process sync, CoW, GUI protocol) — superseded by this file/archives
  wherever they overlap.
