# litebox — current state (2026-09-16)

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

**A trampoline-extension failure used to poison a whole segment's syscalls, now fixed** — sized from a
byte-pair count instead of a one-page guess, capped 4MiB (full detail archived). **Tags, verified live,
never from the name** (full detail archived): `linuxserver/webtop:alpine-mate` ships MATE not XFCE;
`alpine-xfce` doesn't exist; `debian-xfce`/`ubuntu-xfce` ship real XFCE; `edgelevel/alpine-xfce-vnc` is
Alpine 3.16.0.

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

Nine ACK-stall-kill candidates investigated, all refuted or fixed; real blocker was the guest-side
patcher silently crashing on `shutil.copy2()`'s `copystat()`→`os.listxattr()` (no `listxattr` shim)
before ever patching `selkies.py`, fixed via `shutil.copyfile()` plus a backlog-check-ordering fix
(`478e640`) — live-verified 60+s with zero `keepalive ping timeout`. Port-8081 double-bind fix
code-verified + instrumentation-confirmed live (17 boot cycles); the race itself did not recur even
under an escalated 6000-connection stress test. RAM fully recovered on every kill, zero leaks. Full
detail: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

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
per-process `VirtualAlloc2` mechanism for EVERY ordinary host-heap allocation -- exactly as it was
before Track B started. Routing everything (including one-shot buffers like the OCI
rootfs-reconstruction allocation every plain guest exec makes) through the shared bump allocator,
which has no reclaim, was live-proven to exhaust an 8 GiB pool after 45-90 real execs under
`webtop_stack.sh` (`memory allocation of 181493744 bytes failed`, 218 occurrences) -- worse than not
sharing at all. That regression is now gone: **live-verified**, the identical real
`debian-xfce webtop_stack.sh` boot under `LITEBOX_PROCESS_FORK=1` (default flags, sharing gate left
off) ran 200+s, many concurrent forked children, reached `NGINX_STARTED` then the
ALREADY-DOCUMENTED `XVFB_FAILED` architectural gap (below) with **zero** `memory allocation ...
failed` lines (was 218), zero panics/FATAL/abort, host RAM fully recovered on kill.

The fixed-base/atomic-cursor/handle-inherit machinery (`SHARED_KERNEL_HEAP_BASE`,
`shared_heap_cursor`, `shared_kernel_heap_export_for_fork_child`, all live-verified correct in prior
sessions -- 71 concurrent cross-process forks, zero corruption) is NOT deleted: it now backs a small
**64 MiB, standalone, bounded** arena (`shared_kernel_arena_alloc`, `lib.rs`) deliberately NOT wired
to `GlobalAlloc`, reserved for a follow-up session's `LiteBoxX`/`GlobalState`-only migration. Basic
mechanism re-verified live at the new size (parent-side init+map+commit+sentinel-write landed
correctly at `base+64MiB-0x1000`); the opt-in `LITEBOX_DIAG_SHARED_HEAP_INHERIT=1` fork-export path
is untouched and still gated off by default.

### `SharedArc<T>` -- DESIGNED, BUILT, LIVE-VERIFIED cross-process 2026-09-17 (ADVISORY-002 3.3 step 3's final piece)

`litebox_platform_windows_userland/src/lib.rs` (`SharedArcInner`/`SharedArc`, just above
`impl MemoryProvider for WindowsUserland`): a hand-rolled shared-ownership smart pointer over
`shared_kernel_arena_alloc` bytes -- **not `std::sync::Arc`**, whose `ArcInner` layout is a
private std implementation detail (`Arc::from_raw` over manually-placed bytes is unsound), and
stable Rust has no `allocator_api`/`Box::new_in` either. Own `#[repr(C)]` control block
(`strong: AtomicUsize` only -- no `weak`, nothing needs one yet, YAGNI). `SharedArc::new(value)`
returns `(handle, arena_offset)`; `unsafe SharedArc::attach(offset)` (cross-process: a child with
the SAME arena section mapped at the SAME address) increments `strong` and returns an independent
owning handle. `Clone`/`Deref` match `Arc<T>` ergonomics for minimal call-site churn.
**`Drop` deliberately never reclaims or runs `T`'s destructor** (confirmed, not an oversight): (1)
the arena is a pure bump allocator with no free list at all, so freeing bytes is not an available
option regardless; (2) a kernel singleton like `GlobalState`/`LiteBoxX` may embed real per-process
`HANDLE`s/fds, and running its `Drop` from whichever process happens to observe `strong == 0`
would close whatever unrelated handle number is live THERE -- unsound in a multi-process world.
Strong count is still tracked (verification value), reaching zero intentionally does nothing
further -- correct because these singletons are meant to outlive every process in the fork family
for the whole guest session and never really reach zero live anyway.

**Live cross-process proof** (`LITEBOX_DIAG_SHARED_HEAP_INHERIT=1 LITEBOX_DIAG_SHARED_ARC_PROBE=1
LITEBOX_PROCESS_FORK=1`, isolated `SharedArcProbeData{magic, counter: AtomicUsize}` test struct,
riding the existing shared-heap-inherit env-var handoff --
`shared_arc_probe_parent_prepare`/`shared_arc_probe_child_attach`, wired at
`litebox_runner_linux_on_windows_userland/src/lib.rs`'s vmem-adopt-probe-to-task-resume-probe
handoff since ordinary `GlobalAlloc` traffic no longer touches `init_shared_kernel_heap` at all
post-revert, so the child must call it explicitly): real `debian:stable-slim` cheap-repro fork,
one real cross-process child. Parent: `new` -> strong=1, `clone` -> strong=2. Child: `attach` ->
strong=3, `magic` read back byte-identical (`0x5ac55ac55ac55ac5`, proves the child sees the
PARENT's `ptr::write` through the wrapper, not a private copy), `counter.fetch_add` 0->1 (proves
mutation through `Deref`'s interior atomic lands in the SAME physical memory), child `clone` ->
strong=4, child drops that clone -> strong=3. Every number exactly as expected; zero crash, zero
corruption, zero leaked process, host RAM unchanged after run. **Step 1 (ADVISORY-002 3.3) is
done.**

**Step 2 -- create-vs-attach protocol for the REAL `GlobalState`/`LiteBoxX` -- NOT started this
pass, deliberately** (explicit scope call: prove the wrapper first, don't force the migration
unverified). Precisely scoped for the next session: `LiteBox::new` (`litebox/src/litebox.rs:72`)
and `LinuxShimBuilder::build` (`litebox_shim_linux/src/lib.rs:474`) are platform-generic code
shared by every runner (Linux native, macOS, optee, snp, lvbs) -- confirmed live 2026-09-17 that
EVERY process in a fork family, parent and every child alike, independently calls
`shim_builder.build::<DefaultFS<Platform>>()` at its own startup
(`litebox_runner_linux_on_windows_userland/src/lib.rs`'s `diag_process_fork_globalstate_probe`,
the same call site the `[process_fork_diag] globalstate-probe (child): GlobalState constructed
successfully` log line comes from -- this is why merely placing the allocation in shared memory
was never sufficient by itself). Needed: (a) a new trait (alongside `RawMutexProvider`) threaded
through `litebox`/`litebox_shim_linux`'s generic `Platform` bound, real `SharedArc`-backed impl
for `WindowsUserland`, no-op default (ordinary `Arc::new`) for every other platform; (b) a
create-vs-attach branch at that construction call site -- the very first process in a fork family
(never itself a fork child) creates fresh via the new trait/`SharedArc::new`, every
cross-process-fork child instead detects it has an inherited shared-heap section
(`SHARED_KERNEL_HEAP_SECTION_HANDLE`-style env vars already flow today) and `SharedArc::attach`s
to the offset the root process exported, instead of building its own; (c) live proof the SAME
shape as this pass's isolated-struct probe, but for real `GlobalState` -- parent registers
something in a real registry field, a child FORKED AFTERWARD observes it (not a frozen
pre-fork snapshot); (d) only then re-attempt `.wfgy/webtop_stack.sh` under
`LITEBOX_PROCESS_FORK=1` and check whether `XVFB_FAILED`/`DBUS_FAILED` finally resolve. Track as
its own PRD; the arena + `SharedArc` alone do not close the "GlobalState cross-process visibility"
goal, only remove its last soundness blocker.

`XVFB_FAILED`/`DBUS_FAILED` (guest processes share no AF_UNIX/loopback/FIFO namespace -- see "A real
desktop renders in a browser"'s "Open here" note) is UNCHANGED by this pass, exactly as expected:
this fix closes the capacity/OOM regression, not the AF_UNIX-sharing gap, which needs the
create-vs-attach protocol above, not just bytes-in-shared-memory. Full narrative, elimination
trails, both live webtop-boot logs (broken-everything-shared vs. this session's reverted+bounded
run): `docs/AGENTS_ARCHIVE_2026-09-17.md`.

## Closed — do not re-attempt without a genuinely new approach

VEH_FRAME_STRIDE canary guard, `dev_bench`/`litebox_runner_snp` Windows build failures, CoW-mmap
performance, input-latency bugs, presenter-split duplicate-`SYN_REPORT`, and the GUI-protocol
(DRM/KMS+wgpu) decision — all CLOSED 2026-09-16, none open. Full detail moved to
`docs/AGENTS_ARCHIVE_2026-09-17.md` to keep this file under budget.

## Cross-process-fork stdio-handle bug — FIXED 2026-09-17

`spawn_suspended`'s (`litebox_platform_windows_userland/src/process_fork.rs`) two back-to-back
`STARTF_USESTDHANDLES` blocks were NOT merely redundant: the second unconditionally overwrote
`startup_info.hStd*` with **no null/`INVALID_HANDLE_VALUE` guard**, clobbering the first block's
correct "leave this stream unset when invalid" decision. Fixed by keeping exactly one block. Not
independently reproduced (applied on inspection, a real provable defect). Repro note: a bash `-c`
script must end in a trailing command (`OUTER_EXIT=$?`) to force a real `clone()` -- tail-exec of
the final command never calls it. Full detail: archive.

**Second bug found the same pass, since FIXED**: the shared kernel heap's eager-full-commit
defect -- see "Shared kernel heap" section above for the full mechanism and fix.

**PTY test, NOT root-caused**: `script -qec '...' /dev/null` under a real PTY hit `signal=Signal(13)`
on `script` itself ~6s in -- a different bug; PRD `cross-process-fork-pty-sigpipe-in-script-relay`.

**Full webtop boot ATTEMPTED 2026-09-17: blocked by commit exhaustion, since FIXED (see above).**
Stalled at `NGINX_STARTED`'s supervisor retry loop, `ERROR_COMMITMENT_LIMIT` at 96% host commit
charge. Re-run after the fix reached `NGINX_STARTED` again with commit charge held at 39-42%, but
stalled later at `NGINX_SELFTEST_FAILED` (separate, already-tracked nginx issue) before
`XVFB_UP`/`DE_UP`.

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
