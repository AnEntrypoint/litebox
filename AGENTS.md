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
half is real code (`DuplicateHandle`-based) but genuinely untaken today (same-pid always true so
far). Live-verified: `yes hello | head -c 5000000 | wc -c`, `sort --parallel=4` multithreaded
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

## Cross-process-fork stack-overflow class -- ROOT-CAUSED AND FIXED 2026-09-17

The pre-existing, load-scaling `thread '<unknown>' has overflowed its stack` crash above (122
occurrences by `XVFB_FAILED`) is **NOT** stack-size, `fork_verify` single-stepping, or memory
pressure -- every cross-process fork child after the family's first died chasing
`GlobalStateHandle`'s `litebox: LiteBox<Platform>` field: `LiteBox<Platform>` is
`Platform::Handle<LiteBoxX<Platform>>` (an `Arc` pointer), and `GlobalState.litebox` placed that
pointer's literal bytes inline in the cross-process-shared arena at CREATE time -- an ATTACHing
child read back the FIRST creator's pointer VALUE, meaningless in its own address space, and
chasing its garbage `RwLock` internals is what consumed the stack. **This is THE pattern reused
for every fix below**: `GlobalState` loses the field entirely; `GlobalStateHandle` carries its own
copy instead, populated from THIS process's own per-process source on every path (attach or
create) -- Rust's field resolution tries the receiver's own concrete type before auto-`Deref`ing,
so this SHADOWS the removed `GlobalState` field transparently; no external call site needs to
change beyond widening its parameter type from `&GlobalState<..>` to `&GlobalStateHandle<..>`.
Also fixed same-session, same pattern: `proc_self_info`/`pts_registry` (a second,
doc-comment-predicted instance -- `default_fs` mounts `/proc/self`+`/dev/pts` with
`LinuxShimBuilder`'s own per-process copies before `build()`'s attach-or-create decision).

**Live-verified fixed**: two independent full `.wfgy/webtop_stack.sh` boots under
`LITEBOX_PROCESS_FORK=1`, zero `overflowed its stack` occurrences in either (previously 122+ by
`XVFB_FAILED` alone). Full bisection transcript: archive.

## Four more registries/subsystems fixed the SAME session, 2026-09-17 (`34129ed`, `33bc57f`)

With the stack overflow gone, boots progressed further and hit the SAME root cause one field at a
time -- each isolated with a minimal `-Z --oci-image debian:stable-slim -- /bin/bash -c 'mkdir -p
...'` repro under `LITEBOX_PROCESS_FORK=1` (much cheaper than a full webtop boot per iteration),
fixed with the SAME `GlobalStateHandle`-shadow-field pattern as `litebox`/`proc_self_info`/
`pts_registry`, rebuilt, re-verified live after each:

- **`elf_patch_cache`** (`BTreeMap<(pid,fd), ElfPatchState>`): real panic,
  `alloc::collections::btree::node.rs:1232:35`, inside `.entry(...).or_insert(...)`. Keyed by
  `(pid, fd)` already -- no call site reads another process's entry, and `ElfPatchState` holds
  absolute per-process addresses anyway, so per-process storage is CORRECT, not just safe.
- **`exec_ranges_cache`** (`BTreeMap<(dev,ino), Arc<Vec<Range<u64>>>>`): same panic signature,
  next field down, once the above was fixed. Values are a pure function of a file's own ELF
  section headers -- per-process storage just re-derives them; only cross-process cache reuse is
  lost.
- **`segment_scan_cache`** (`BTreeMap<SegmentScanKey, Arc<SegmentScanTemplate>>`): with the above
  two fixed, the repro stopped panicking but HUNG instead (host CPU climbing, zero new log
  output) -- a corrupted `BTreeMap` can walk into a long/cyclic chain instead of an
  out-of-bounds `unwrap`. Same fix; re-verified past this cache.
- **Trampoline placement** (not a `GlobalState` registry -- a genuinely different subsystem, found
  immediately after the three caches above stopped blocking ELF loading): `maybe_patch_exec_segment`'s
  fallback, when `MAP_FIXED_NOREPLACE` at the ELF-computed preferred trampoline address fails
  (common in a cross-process-fork child, whose adopted VMA layout starts far denser than a fresh
  process's), called `do_mmap_anonymous(None, ...)` -- discarding the proximity hint entirely.
  `Vmem::get_unmmaped_area` has no "nearby" concept for an occupied non-fixed hint (silently
  ignored, falls through to a fully generic top-down/gap search -- see its own "1.5 HELD BACK"
  comment, a related but different, deliberately-still-disabled fix), so the chosen address could
  land anywhere in the guest's whole address space -- live-caught landing ~127 TiB from the code
  segment, `distance > 0x7FFF_0000` (JMP rel32 range), triggering `apply_trap_fallback` (poisons
  every `syscall` in that segment to a crash trap) and killing the guest the moment it executed
  one. Fixed by `Task::probe_nearby_trampoline_slot` (`litebox_shim_linux/src/syscalls/mm.rs`): a
  LOCAL, bounded probe (real `MAP_FIXED_NOREPLACE` attempts at exponentially-increasing offsets on
  alternating sides of the preferred address, capped at 24 rounds) scoped to just this one call
  site, not a change to the shared `get_unmmaped_area` every `mmap()` goes through. Live-verified:
  zero `trampoline too far` occurrences over a 5-iteration mkdir loop that previously hit it on
  literally every single `execve` (100% occurrence rate before the fix).

**Does NOT close `XVFB_FAILED`/`DBUS_FAILED`. Real next blocker, NOT YET root-caused (unlike the
four fixes above, which all have an exact panic backtrace or a 100%-vs-0% before/after measurement)**:
with all four landed, both the minimal mkdir repro AND a full `.wfgy/webtop_stack.sh` boot show a
NEW failure shape -- intermittent, no panic, no host AV diagnostic:
- Some forked children complete cleanly (`run_thread returned (guest thread terminated)`, clean
  exit).
- Some are silently killed mid-syscall with zero diagnostic output of any kind (live-caught:
  `mkdir`'s own guest tid, mid a `sys_read` loop over an mmap'd library file at climbing 4096-byte
  offsets -- reads simply stop, no error, no panic, shell reports `Killed`).
- Some hang instead: `entering real guest execution` logged, then zero further output while host
  CPU climbs continuously (same signature as the `segment_scan_cache` hang above, before it was
  fixed) -- reproduced both on the minimal mkdir repro's later fork iterations AND on the full
  webtop boot's very FIRST `/webtop_stack.sh` guest thread (before any `[s]` marker), each killed
  manually after CPU kept climbing with no new log line.

**Leading hypothesis, not yet confirmed**: one of the six registries still genuinely shared and
still real per-process-heap `BTreeMap`s -- `pty_registry`, `daemon_pty_masters`, `flock_registry`,
`fifo_registry`, `sysv_shm`, `memfds`, `shared_files` (full type list: this file's "Shared kernel
heap" section below) -- now being reached for the first time now that ELF loading no longer blocks
earlier. **Unlike the four fixes above, these genuinely NEED real cross-process sharing for correct
Linux semantics** (a pty id, a file lock, a FIFO, a SysV shm segment, a `MAP_SHARED` file mapping
must be visible to the rest of the fork family) -- the `GlobalStateHandle`-shadow-field pattern used
above is WRONG for them; each needs the SAME flat, pointer-free redesign
`SharedUnixAddrPresenceTable` already proves out (below), separate engineering work per registry.
Plain `mkdir` does not obviously touch any of these, so this hypothesis is NOT confirmed -- the
intermittent, no-diagnostic nature (vs. the four fixes above's deterministic, exact panics) suggests
a corrupted `BTreeMap` reached only via a racy/address-layout-dependent path, OR a different
mechanism entirely. **Next session: get an exact backtrace or crash signature for THIS failure
before fixing anything** -- same discipline that made the four fixes above fast and certain, don't
guess which registry this time. `RUST_BACKTRACE=1` did not catch it (no panic fires); a live
debugger attach or a narrower bisection (bisect by disabling one registry's real use at a time,
same technique used to find `litebox` originally) is the likely next tool needed.

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
