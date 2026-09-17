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
146-152`). Do not cite the separate curl-self-test stall as live open work: that one is fixed.

**Fork-after-Xorg PERMANENT freeze — did NOT reproduce 2026-09-17; live evidence says it is
thread-based-fork-only.** The archived repro now hits the ALREADY-DOCUMENTED "second glibc
corruption class" (`double free or corruption (out)`, see "still open" above) before Xorg survives
long enough to reach the freeze precondition. **Decisive substitute test**: the identical script
with `LITEBOX_PROCESS_FORK=1` as a real host env var completed cleanly 2/2 — zero freeze, zero
double-free, consistent with the freeze being thread-path-specific. Full evidence, both repro logs,
and a disclosed ENOMEM finding under concurrent cross-process forks: `docs/AGENTS_ARCHIVE_2026-09-17.md`.

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

**The glibc/tcache crash class still sporadically hits selkies**, separately from the ACK-stall-kill:
live-captured once despite the `--env` tunables flag being passed correctly (a DPI-fork on a client's
5th rapid reconnect SIGSEGVs) — genuinely ADVISORY-001 §3N on selkies' own fork; not yet re-verified
crash-free over many cycles (the ACK-stall-kill dominates the symptom in practice). **2026-09-16:
`GLIBC_TUNABLES` propagation through `spawn_exec_collision_child` has NO gap** (live-proven, true on
every collision including selkies' own python3 re-exec) — **the recurring crash is a SECOND, different
corruption signature under heavy fork load** (`double free or corruption (out)` SIGABRT, not §3N's
`REVEAL_PTR` XOR SIGSEGV), hitting bin paths the tunables deliberately leave enabled. **Track B
territory, not a tunable-coverage gap** — do not re-attempt a `GLIBC_TUNABLES`/env fix without evidence
of a THIRD mechanism. Full evidence: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

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

## Cross-process-capable `RawMutex` (Track B step 2, ADVISORY-002 §3.2) -- done, live-verified

`litebox_platform_windows_userland/src/lib.rs`'s `RawMutex` no longer calls
`WaitOnAddress`/`WakeByAddressSingle` (process-local per MSDN, see "hard platform constraint"
above) -- replaced with a manual wait queue plus one auto-reset kernel `Event` per OS thread. Same
trait/`underlying_atomic()`/`INIT` contract, no caller changed; `wake_many` now returns the real
popped-waiter count (was always `0` -- a pure improvement, not a behaviour requirement change).
Full internals (queue/lock-ordering, timeout-race resolution): `docs/AGENTS_ARCHIVE_2026-09-16.md`.

**Cross-process half is real code, not a stub, but genuinely untaken today**: every
`WaiterRecord` carries the waiter's pid; same-pid (always true today) uses the handle directly, a
different pid would use `DuplicateHandle` (already proven live cross-process, non-admin) cached in
`remote_waiter_handles`. Deliberately different from `xproc_sync.rs`'s single named-per-mutex
event (needs a section offset to key its side-table by, i.e. step 3).

**Live-verified** (release build, default thread-based fork, no test files): `yes hello | head -c
5000000 | wc -c` -- exact `5000000`. `seq 1 3000000 | sort --parallel=4 -n | tail -3` -- exact
correct output, proving `sort`'s real multi-threaded pthread mutex/condvar contention completes
with no hang/deadlock/missed-wakeup/corrupted-merge. Host RAM identical before/after.

**3-stage-pipeline SIGPIPE: relay EXONERATED 2026-09-16** -- `seq 1 200000 | sort -n | tail -3`
under `LITEBOX_PROCESS_FORK=1` truncates upstream of the relay (guest execution correctness, not
the relay: `total_read == total_written` every time, ~9 live runs). PRD
`process-fork-pipe-relay-sigpipe-above-4kb` resolved (redirected); don't rely on
`LITEBOX_PROCESS_FORK=1` for heavy-iteration guests. Full repro/evidence: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

## Shared kernel heap -- SELECTIVE-ROUTING CORRECTION LANDED 2026-09-17 (ADVISORY-002 §3.3)

**The "route everything through one shared section" design (Track B steps 3-5, `c08182d`..`3d661d2`)
is REVERTED.** `SLAB_ALLOC` (`#[global_allocator]`, `lib.rs`) is back to the pre-`c08182d` private
per-process `VirtualAlloc2` mechanism for every ordinary host-heap allocation. Routing everything
through the shared bump allocator (no reclaim) was live-proven to exhaust an 8 GiB pool after 45-90
real execs (`memory allocation ... failed`, 218 occurrences) -- worse than not sharing at all;
**live-verified fixed**, zero such failures on an identical re-run. The fixed-base/atomic-cursor/
handle-inherit machinery is NOT deleted -- it now backs a small **64 MiB, standalone, bounded**
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
branch. **Decisive live proof**: parent bumps `next_thread_id` by a sentinel delta both immediately
before AND strictly after `spawn_cross_process_fork_child` returns; the child's own post-`build()`
read observes both bumps -- only possible if it is the SAME live allocation, not a frozen snapshot
or a merely-consistent-address independent copy.

**Does NOT close `XVFB_FAILED`/`DBUS_FAILED`: root cause precisely characterized.** `SharedArc::new`
places only `T`'s literal inline bytes in the arena -- fine for plain scalars/an inline sync word,
but every `GlobalState` REGISTRY (`unix_addr_table`, `pty_registry`, `daemon_pty_masters`,
`flock_registry`, `fifo_registry`, `sysv_shm`, `memfds`, `shared_files`, 3 caches) is a
`BTreeMap`/similar whose NODES live on the ordinary private per-process heap -- an attaching
process's copy of the root pointer is meaningless in its own address space. Follow-up PRD:
`globalstate-nested-collections-not-actually-shared` (**`unix_addr_table` specifically since
partly closed -- see the section below**). Full mechanism, every registry's exact type, the
`proc_self_info`/`pts_registry` mount-ordering caveat: `docs/AGENTS_ARCHIVE_2026-09-17.md`.

## `unix_addr_table` presence sharing -- landed and live-verified; does NOT close `XVFB_FAILED`
## (real blocker identified: a pervasive, PRE-EXISTING, load-scaling stack-overflow crash)

Scoped follow-up, JUST `unix_addr_table` (other 5 registries: still untouched).
`litebox_shim_linux/src/syscalls/unix.rs`'s `SharedUnixAddrPresenceTable`: fixed-256-slot, pure-
`core::sync::atomic` (zero `unsafe`, zero `RawMutex` -- its bookkeeping `Mutex<Vec<..>>` is itself
per-process, not actually cross-process-safe today, see archive), lock-free side-index recording
`(kind, key bytes <=108, owner guest pid)` per bind/listen, mirrored alongside (never replacing)
each process's real `unix_addr_table` `BTreeMap`. A plain `GlobalState` field (`unix_addr_presence`,
no new `SharedKernelStateProvider` slot needed): zero pointer indirection, so it inherits whatever
sharing `GlobalState` itself already has for free, same mechanism as `next_thread_id`. Wired at all
4 call sites (stream `listen`/`Drop`, datagram `bind`/`Drop`) plus an always-on diagnostic on every
real `ECONNREFUSED`, distinguishing "nothing listening" from "listening, in a DIFFERENT guest pid,
not yet reachable" (`[unix_addr_presence]` log line).

**Decisive live proof** (`LITEBOX_DIAG_UNIX_ADDR_PRESENCE_PROBE=1`, mirrors `GLOBALSTATE_SHARE_PROBE`
exactly): parent registers one key immediately before `spawn_cross_process_fork_child`, a SECOND
key strictly AFTER it returns; child looks up both right after its own `build()`. Live result:
`child observed before=Some(1) after=Some(1)` -- proves genuine live sharing, not a snapshot.

## Cross-process-fork stack-overflow class -- ROOT-CAUSED AND FIXED 2026-09-17

The pre-existing, load-scaling `thread '<unknown>' has overflowed its stack` crash above (122
occurrences by `XVFB_FAILED`, previously blamed on `xset q`/X11 specifically and suspected
host-memory-pressure-driven) is **NOT** stack-size, `fork_verify` single-stepping, or memory
pressure -- live bisection (temporary log markers, since removed) proved EVERY cross-process fork
child after the first (trivial `mkdir`/`rm -rf` as readily as `xset q`) died inside
`GlobalStateHandle`'s `litebox: LiteBox<Platform>` field's `descriptor_table_mut()`/`RwLock`
machinery. Real mechanism: `LiteBox<Platform>` is `Platform::Handle<LiteBoxX<Platform>>` (an `Arc`
pointer); `GlobalState.litebox` used to place that pointer's literal bytes inline in the
cross-process-shared kernel arena at CREATE time. A LATER cross-process-fork child that ATTACHES
(every fork after the family's first) read back the FIRST creator's pointer VALUE -- meaningless
in its own address space -- and chasing its garbage `RwLock` internals is what actually consumed
the stack (unbounded, since the "loop" is walking corrupted memory, not bounded guest work), not
guest instruction count. Same defect class already documented below for `unix_addr_table` et al.,
just never previously found in `litebox` itself.

**Fix** (`litebox_shim_linux/src/lib.rs`): `GlobalState` no longer has a `litebox` field (nor
`proc_self_info`/`pts_registry`, a second, doc-comment-predicted instance of the identical defect
-- `default_fs`/`default_fs_multi_layer` mounts `/proc/self`+`/dev/pts` with `LinuxShimBuilder`'s
own per-process copies BEFORE `build()`'s attach-or-create decision, so an attaching child's
`GlobalState` copy was likewise always the wrong, foreign-process one). `GlobalStateHandle` now
carries its own `litebox`/`proc_self_info`/`pts_registry` fields, populated from THIS process's own
`LinuxShimBuilder` fields on every path (attach or create) -- Rust's field resolution tries the
receiver's own concrete type before auto-`Deref`ing, so this SHADOWS the removed `GlobalState`
fields transparently; no external call site (185+ `xxx.litebox`/`.proc_self_info`/`.pts_registry`
uses across `epoll.rs`/`net.rs`/`pipe.rs`/`pty.rs`/`file.rs`/`unix.rs`) needed to change beyond
widening their `&GlobalState<Platform, FS>` parameter/`impl` types to `&GlobalStateHandle<Platform,
FS>` (a pure widening -- `GlobalStateHandle` derefs to `GlobalState`, so every other field/method
access on those same parameters is unaffected). `litebox::LiteBox::clone` widened from
`pub(crate)` to `pub` (litebox_shim_linux is a legitimate, now-documented user, not the "outside
user" that visibility was guarding against).

**Live-verified fixed**: two independent full `.wfgy/webtop_stack.sh` boots under
`LITEBOX_PROCESS_FORK=1`, zero `overflowed its stack` occurrences in either (previously 122+ by
`XVFB_FAILED` alone) -- confirmed by `grep -c` over each full log. `fork_verify` wiring unchanged
(an A/B with it disabled entirely hit the identical crash, ruling it out). The 32 MiB
guest-execution thread wrap in `diag_process_fork_globalstate_probe` (matching every other
guest-executing thread's stack-size pattern) is kept -- independently correct even though it
wasn't sufficient alone. Full bisection transcript, both ruled-out hypotheses: archive.

**Does NOT close `XVFB_FAILED`/`DBUS_FAILED`: a DIFFERENT, already-documented gap is next.** With
the stack overflow gone, boots now progress substantially further before hitting the SAME root
cause this section already names below for `unix_addr_table` et al. -- a clean, host-diagnosed
`STATUS_ACCESS_VIOLATION` in `<litebox::fs::procfs::ProcSelfTable>::set` on the FIRST run (before
the `proc_self_info` fix landed) and, after it, a `BTreeMap` navigation panic
(`alloc::collections::btree::navigate.rs`, `Option::unwrap()` on `None`) in one of the remaining
shared registries (`unix_addr_table`/`pty_registry`/`daemon_pty_masters`/`flock_registry`/
`fifo_registry`/`sysv_shm`/`memfds`/`shared_files`/2 caches -- exact field not yet isolated).
**Unlike `litebox`/`proc_self_info`/`pts_registry`, these registries genuinely NEED real
cross-process sharing for correct Linux semantics** (a listening AF_UNIX socket, a file lock, a pty
registration must be visible to the rest of the fork family) -- the `GlobalStateHandle`-shadow-
field fix used above is WRONG for them (it would silently make them non-shared, reintroducing the
exact bugs today's `unix_addr_table` presence-table work exists to fix). The real fix per registry
needs the SAME flat, pointer-free redesign `SharedUnixAddrPresenceTable` already proves out (below)
-- real, separate, per-registry engineering work, correctly scoped as "next session" already.

**Next session, in order**: (1) identify exactly which registry's `BTreeMap` panicked (temporary
per-registry access logging, same bisection technique used to find `litebox` above); (2) apply the
`SharedUnixAddrPresenceTable` flat-table pattern to it; (3) repeat for the remaining registries one
at a time, live-testing `.wfgy/webtop_stack.sh` after each; (4) only once ALL of them are
genuinely shared does a live desktop/browser/Terminal-Emulator/Thunar test become meaningful.
Full bisection methodology, live proof transcripts: `docs/AGENTS_ARCHIVE_2026-09-17.md`.

## Closed — do not re-attempt without a genuinely new approach

VEH_FRAME_STRIDE canary guard, `dev_bench`/`litebox_runner_snp` Windows build failures, CoW-mmap
performance, input-latency bugs, presenter-split duplicate-`SYN_REPORT`, and the GUI-protocol
(DRM/KMS+wgpu) decision — all CLOSED 2026-09-16, none open. Full detail moved to
`docs/AGENTS_ARCHIVE_2026-09-17.md` to keep this file under budget.

## Cross-process-fork stdio-handle bug — FIXED 2026-09-17

`spawn_suspended`'s two back-to-back `STARTF_USESTDHANDLES` blocks clobbered each other (no null
guard on the second); fixed by keeping exactly one. PTY test (`script -qec ...`) hit a separate,
NOT-root-caused `signal=Signal(13)` -- PRD `cross-process-fork-pty-sigpipe-in-script-relay`. Full
detail, including the commit-exhaustion boot attempt this pass also fixed: archive.

## Presenter-process split -- done, fully verified live end-to-end, 2026-09-16

`litebox_presenter_protocol` crate + runner-side `ControlServer` (zero-copy scanout handoff) +
`litebox-presenter.exe`; `--gui` is now `Option<GuiMode>`. One real bug found+fixed (missing
per-call `OVERLAPPED`). Full narrative: `docs/AGENTS_ARCHIVE_2026-09-16.md`,
`docs/presenter-process-design.md`.

## Five cheap-wins PRD rows closed, 2026-09-16

Cargo build/fmt-verified, no boot needed. Full detail: `docs/AGENTS_ARCHIVE_2026-09-17.md`.

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
- `docs/macos.md` — port state; the Apple Silicon guest-execution context switch is a stub, stays
  deferred (PRD `macos-aarch64-guest-execution-context-switch-is-not-implemented`,
  `gui-macos-presentation-runner-and-guest-entry-blocked`). Probe crates: `docs/wayland-drm-backend-probe/`,
  `docs/linux-native-drm-gui-probe/`.
- `docs/presenter-process-design.md` -- IMPLEMENTED and fully live-verified 2026-09-16; see this file's
  own "Presenter-process split" section above. Designs NOT implemented: `docs/session-daemon-design.md`
  (`litebox_termemu`'s VT100-emulator slice IS implemented; the daemon/IPC layer is not),
  `docs/fork-region-grouping-design.md` (still a diagnostic probe).
- `advisor/probes/` — diagnostics (`decode_frame.py`, `symbolize_litebox_crash.py`, `dup_probe.c`,
  `drm_flip_probe.c`, `clone_probe.c`) plus `MEASUREMENT-PITFALLS.md`, `DISK-HYGIENE.md`. OCI-pull
  Python scripts there are retired.
- `.gm/memories/` holds older per-topic notes (RtlpUnwindPrologue, browser witness, XFCE/MATE/weston,
  packager OOM, image tags, cross-process sync, CoW, GUI protocol) — superseded by this file/archives
  wherever they overlap.
