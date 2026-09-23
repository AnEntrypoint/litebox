# litebox — current state (2026-09-23)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, or one a later commit superseded, is deleted rather than hedged. Reference
detail is drained to `docs/AGENTS_ARCHIVE_*.md` and dated `docs/*.md` in the map below — read those
for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one
line plus its pointer, not a separate memory file. Compacted at the 65th, 70th, 72nd, 75th, 76th,
81st, 83rd, 85th and 88th passes (pass-history section below; 26th-69th full narrative:
`docs/AGENTS_ARCHIVE_2026-09-22.md`; 70th-87th full narrative, including each pass's own complete
evidence and fix rationale: `docs/AGENTS_ARCHIVE_2026-09-23.md`). Re-compacted 88th pass (drained
the 83rd-87th passes' own full bug-by-bug writeups to the archive, now that
`lazy_fork_commit.rs`'s own module doc comment serves as the canonical detailed record for that
mechanism; ~53.6KB→~44KB, still above the 30KB threshold — further compaction of older,
already-CLOSED sections is the next pass's own pickup if this file crosses 30KB again).

## The cheap repro — start here

```
target/release/litebox_runner_linux_on_windows_userland.exe -Z \
  --oci-image docker.io/library/debian:stable-slim -- /bin/bash -c '<script>'
```

One ~81MB layer, `[cache] HIT` after the first pull, real GNU coreutils instead of busybox (coreutils
`touch` issues the `utimensat`/futimens form busybox's never reaches, `caaac79`). Host-side gotchas:

- **PowerShell, never Git Bash** — Git Bash rewrites `/absolute/guest/paths` into `C:/Program
  Files/Git/...` before the runner sees them (misleading `ENOENT`). `Start-Process
  -RedirectStandardOutput/-RedirectStandardError` makes the runner exit almost instantly with zero
  guest output; use `& .\runner.exe ... *> combined.log` instead.
- **Single quotes only inside `-c`** — embedded double quotes are corrupted crossing into the
  child's Win32 command line (masqueraded as deep fork/stack-pointer corruption for a whole session).
- **`LITEBOX_PROCESS_FORK=1` is a HOST env var, not a guest `--env`** —
  `spawn_cross_process_fork_child` (`litebox_platform_windows_userland/src/lib.rs`) reads it via a
  bare `std::env::var_os` on the HOST side; via `--env` it silently no-ops with ZERO log output
  (looks like "not eligible" but isn't even attempted) — 37th pass.
- **Boot logs from PowerShell redirection (`*> file.log`) are UTF-16LE, not UTF-8** — a plain
  `grep`/`Select-String` silently returns zero matches even when the text is there. Always `iconv
  -f UTF-16LE -t UTF-8` (or `Get-Content -Encoding Unicode`) first — 53rd pass.
- **`*> file.log` WORD-WRAPS any tracing line longer than ~116-119 chars across MULTIPLE physical
  lines, no continuation marker** — data isn't lost, only split (repro: `python -c
  "sys.stderr.write('A'*300)" *> f` → 3 physical lines for 1 logical write). A naive line-based
  grep/regex over a long DEBUG line sees only the first ~116-119 chars (70th). **Fix**: a physical
  line NOT starting with the `<float>s` timestamp prefix is a continuation — rejoin before regex.

**Log level**: default is `warn,litebox_platform_windows_userland::fork_verify=error` (`fork_verify`
pinned to `error` since it warns per single-stepped instruction). Don't add `LITEBOX_LOG=error` by
reflex; use `fork_verify=warn` for a fork heal. Bare `LITEBOX_LOG=debug` is useful but too noisy for
a full boot. **Prefer the two dedicated low-overhead targets over any blanket module target**:
`litebox_diag::process_timeline=debug` (five `DIAG_TIMELINE` lines, system-wide, cheap) and
`litebox_diag::socket_read=debug` (read()/recvfrom() payload previews, narrow with
`LITEBOX_DIAG_SOCKET_READ_TARGET=<comm>`, unset = every process, 74th) — both far cheaper than the
old `litebox_shim_linux::syscalls::{process,net,file,unix}=debug` recipe (~70+ call sites/module
flooded across every concurrently-forked process during a boot's fork storm).

## Standing lessons and hard constraints

- **No WSL/hypervisor ever** — run under the matching runner (`litebox_runner_linux_on_windows_
  userland.exe`/`litebox_runner_linux_userland`); cross-compiling FOR Linux is fine, running the
  result in a VM defeats the premise.
- **`fork_verify.rs`'s stale-pointer-healing bug class is Windows-only** (real `fork()` gives
  identical child addresses) — never port it to another platform's crate.
- **Never `bcdedit /debug on`** without a kernel debugger attached — two full-host freezes so far.
- **A process spinning in a dead-locked allocator/spinlock resists `Stop-Process -Force`** — use
  `Invoke-CimMethod -MethodName Terminate` (WMI). `cdb -p <pid>` must use `-pv`/`qd`, never bare `q`
  (kills the target).
- **Never run two full-stack verifications concurrently** — starves both, looks like a real hang.
  Kill every `litebox_runner` between runs; watch `FreePhysicalMemory`, kill on a falling trend.
- **`LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check**, never `PrintWindow`/
  `CopyFromScreen` — decode via `advisor/probes/decode_frame.py`, correlate against `DIAG_TIMELINE
  execve`'s real argv0.
- **Never time litebox with one host process per datapoint** (bare spawn costs 1.6-2.3s) — run N
  iterations in ONE guest process; never subtract timestamps across a parent log and a fork-child
  log (`init_logging()` resets elapsed time to ~0 per child).
- **Release-binary `cdb` reads are unreliable** (MSVC ICF folds distinct functions into one symbol)
  — build `cargo build -p litebox_runner_linux_on_windows_userland` (no `--release`) for any `cdb`
  session needing a trustworthy stack.
- **Refusal errno choice is API contract** — EPERM lets callers degrade, EINVAL/ENOSYS fails them
  hard; wrong choices have silently broken whole subsystems before (30th-pass AF_UNIX
  `EAGAIN`-vs-`EINPROGRESS` fix is the newest instance).
- **Proving a run took the cross-process fork path needs `[process_fork_diag] task-resume-probe`
  lines, never the shim's eligibility log** — the latter fires regardless of outcome (archive).
- **An fd subsystem being "uncarriable" across a cross-process fork doesn't mean the fork must be
  refused** — pipes/regular files/eventfds ARE carried; close-on-exec and pty fds are safely
  DROPPED and the fork proceeds (a pty, unlike a socket, is RE-OPENABLE by id via `SharedPtyTable`);
  only genuinely unrecoverable kinds (unix-socket) refuse. Check `try_cross_process_fork`'s match
  arms (`litebox_shim_linux/src/syscalls/process.rs`) before assuming a new kind needs old
  treatment. Run dbus-daemon non-forking; for XFCE use `xfce4-session`, never `startxfce4`.
- **`wait4()`/`kill()` to a cross-process fork child are asymmetric** — `kill()` to a
  `cross_process_children`-tracked pid returns `ESRCH` unconditionally (documented gap: reachable
  via `wait4`, just not signalable yet).
- **A `socketpair(2)`-originated fd (both ends `Unnamed`) is NOT safe to drop as CLOEXEC across a
  cross-process fork** — real processes (`dbus-daemon`'s babysitter) use it for pre-`exec()`
  bookkeeping; `raw_fd_is_addressless_unix_socket_pair` (`net.rs`) refuses it (falls back to
  thread-based fork) rather than silently dropping it (54th).
- **A `TypedFd`'s index is only valid against the SAME `Descriptors` instance that `insert()`ed
  it** — reading one back against a different process's table is out-of-bounds or resolves to an
  unrelated entry; every accessor in `litebox/src/fd/mod.rs` returns `None` rather than panicking
  (`faa74c6`) — same class as the `Network::queued_for_closure`/`Pipes.litebox`/`FutexManager` bugs.
- **A `de_only.sh`/`LITEBOX_PROCESS_FORK=1` boot's RAM floor is NOT a fixed ~3.1-3.3GB plateau — it
  depends on concurrent HOST load, can fall well below 1GB free** (73rd, 4/4 `de_only_xcensus_
  seed2.tar` boots: collapse to 500MB-1.5GB free within ~10-20s of `xfce4-session`'s fork tree
  starting). `taskkill /IM litebox_runner…exe /F /T` reliably recovers RAM even from <500MB-free;
  watch `FreePhysicalMemory` throughout, kill on a FALLING TREND not a fixed number. **Confirm the
  release binary's mtime postdates the newest relevant commit before trusting a boot result** (57th
  caught a ~1hr-stale binary this way). See Track B item 1.
- **`de_only_xcensus_seed2.tar` (NOT `de_only_seed.tar`) reaches `DE_LAUNCHED_DIRECT` in ~10-15s and
  does NOT hit the 71st-pass `gpg-agent`/`iceauth`/`ssh-agent` dead end** (4/4, 73rd) — preferred
  harness, ~10x faster to `xfwm4`-launch than `webtop_stack.sh`. **`de_only_xcensus_seed3.tar`**
  (75th, disk-only) is the same seed with its `/tmp/xcensus.py` round trip rewritten to feed
  `python3` via stdin instead of a `/tmp` file — seed2's census always failed `rc=2` ENOENT
  (writable-layer-visibility gap on `/tmp`); seed3 returns real data (`rc=0`). Use seed3.
- **A bare file redirect (`cmd > /tmp/f` + a later sibling's read) used to fail silently under
  `LITEBOX_PROCESS_FORK=1`, same root cause the 75th pass fixed (`1d449e6`)** — not yet re-verified
  for a literal `>` specifically, so prefer a PIPE or `$( )` when in doubt: `cmd 2>&1 | sed
  's/^/[tag] /' &` for streaming, `VAR=$(external-cmd)` for captured output (44th-pass fix).
- **`.wfgy/webtop_stack.sh` is NOT what boots — `.wfgy/webtop_seed.tar` embeds a FROZEN COPY**
  (`--resume-from`); editing the host script alone changes nothing. Re-tar after every edit (stage,
  overwrite, `tar -cf webtop_seed.tar webtop_stack.sh tmp config`), verify with `tar -xOf ... |
  grep`. Found the hard way: the 35th pass's `/dev/tcp` rewrite was still absent from the tar on
  the 38th pass — never once ran in a guest.
- **A boot whose log stops is usually a DEAD ROOT RUNNER, not a hang** — when the root process dies,
  `[s]` markers stop while orphaned cross-process children (Xvfb, selkies) keep burning CPU, reading
  like a stall. Check `Get-CimInstance Win32_Process -Filter "Name='litebox_runner…'"`'s
  `CommandLine.Length` — a cross-process CHILD has the bare 77-char exe-only command line
  (`process_fork.rs:1594-1599`); if no survivor carries the full `--oci-image…` args, the root is
  gone (RAM pressure → OOM-kill).
- **Before ANY `cdb` attach, set `LITEBOX_DIAG_NO_EXTERNAL_FAULT_WATCHDOG=1` and
  `LITEBOX_DIAG_NO_FAULT_WATCHDOG=1`** — every runner spawns a watchdog (`process_fork.rs:4391`)
  that `TerminateProcess`es after 15s of <10ms CPU delta, killing a debugger-frozen target.
- **Socket read/write tracing (69th-74th; full mechanism: archive)**: `sys_write`/`sys_writev` log
  under `syscalls::file`, not `net` (`file.rs:1847`/`2990`). A socket fd's `read(2)`/`readv(2)` is a
  SEPARATE path from `recvmsg(2)` — real Xlib/XCB Xtrans uses plain `read()`/`write()` (root cause,
  71st, of the 70th pass's X11-reassembly desync). `run_on_raw_fd` (`lib.rs:1702`) splits socket fds
  into `net` (generic TCP) and `unix` (`UnixSocketSubsystem`, what X11/D-Bus use) — the 71st pass's
  `litebox_diag::socket_read` diagnostic only instrumented `net` (fixed 73rd, `fc830d1`, mirrored
  onto `unix`). Blanket `syscalls::file=debug` is unusable on a real boot (50MB+/s of guest time,
  destabilized a boot enough to break `xdpyinfo`, 71st) — use `litebox_diag::socket_read` instead,
  optionally `LITEBOX_DIAG_SOCKET_READ_TARGET=<comm>[,<comm>...]` (74th; unset = every process).
- **`FlushingStderr` (`litebox_runner_linux_on_windows_userland/src/lib.rs`) does one locked
  `write_all`+`flush` per tracing EVENT, in `Drop`** — closes a real interleaving window across
  concurrent guest (=Windows) threads the prior separate lock/write/lock/flush had. Fixed; NOT the
  70th pass's X11-reassembly desync explanation (that gap was the `read()`/`recvmsg()` split above).
- **All five `DIAG_TIMELINE` sites log at `debug!` on their own `litebox_diag::process_timeline`
  target**, not nested under `syscalls::process`/`syscalls::signal` (~70 unrelated sites each; 74th)
  — use `LITEBOX_LOG=warn,litebox_platform_windows_userland::fork_verify=error,litebox_diag::
  process_timeline=debug` for cheap whole-boot coverage. A cross-process fork child's guest pid IS
  its real Windows PID (`runner…/lib.rs:1673`), so `DIAG_TIMELINE execve`'s `pid=` is `cdb -pv -p`-able.
- **On host-side crashes, use `advisor/probes/symbolize_litebox_crash.py`, snapshotting `.exe`+`.pdb`
  next to the log** — a ring dump's `rva=` is only meaningful against the exact emitting build.
- **Isolate the harness before blaming litebox** — launch guest probes directly as the runner's
  top-level program, never via a runtime-built `/bin/sh -c` wrapper. Never trust a container tag
  name for its WM/session contents — verify by registry manifest + blob tar-listing or a live
  in-guest `/usr/bin` listing. Never record a test count not watched run to completion; never leave
  a suite red for an environmental reason. **The 53rd pass's "`xfce4-session` startup depth varies
  run to run" was itself a RAM-exhaustion artifact (fixed 56th/57th)** — 58th pass's 3/3 clean runs
  all reach the identical depth (`iceauth`+`ssh-agent` spawned, then hangs — Track B item 1).
- **Repo hygiene** — packed layer tars, frame dumps and debug logs never go in git (`.wfgy/`,
  gitignored); untrack anything `git add -A` sweeps.
- **Guest-reachable code returns an errno, never a panic** — the host process IS the entire guest
  session, so an `unimplemented!()`/`unreachable!()`/panic, or unbounded recursion, on any
  guest-reachable path kills every guest process at once (OOM, metadata ops, open flags, nested
  `epoll_ctl`, corrupted guest contexts — full fixed-bug list with shas: archive).

## Cross-process fork (`LITEBOX_PROCESS_FORK=1`)

A genuine `D == 0` fork — child at the SAME addresses, no relocation, no `fork_verify` healing —
exists as `spawn_cross_process_fork_child` (`advisor/ADVISORY-002-d-zero-fork.md`), short-circuiting
to a native fork when available (the fd-carrying apparatus is Windows-only scaffolding for a
missing syscall). **Correctness-sound**: zero corruption on a `bash -c` loop repro vs the
thread-based default's 100% tcache-corruption rate (ADVISORY-001 §3N is thread-path-only).

**Eligibility** — an already-borrowed fd table, a beyond-stdio fd that isn't a pipe end/path-recorded
regular file/eventfd/close-on-exec/pty (overridable by `LITEBOX_PROCESS_FORK_IGNORE_FDS`), or an
unsanitizable `fs_base`/context. No by-name gate exists (34th) — only this global opt-in env var
plus the per-fork fd-kind scan; the only remaining blocking kind on a real `debian-xfce` boot is
`unix-socket`. Fork-child GPR/vmem-adopt cost is small (~1.2s, down from ~3.5-5s); the rootfs
index-merge cost the 56th pass fixed is NOT the dominant per-fork cost any more (confirmed 100%
cache-hit, `LITEBOX_DIAG_FORK_TIMING=1`, 75th: real per-fork rootfs cost ~83-140ms). The real
per-fork-count cost is each fork being a separate Windows process with its own ~350MB-1.1GB peak
working set (guest-memory emulation, writable-layer import, rootfs materialization — not yet
decomposed; see Track B item 1). **`live_cross_process_fork_children`** (`GlobalState` field,
`litebox_shim_linux/src/lib.rs`, 76th) is admission control capping concurrent cross-process-fork
children at 6 (`Task::reserve_cross_process_fork_slot`/`release_cross_process_fork_slot`,
`syscalls/process.rs`, fails open ~8s) — real but only PARTIAL mitigation, see Track B item 1.
Still open: nginx's own SSL-cert generation fails on its first startup attempt, not root-caused
(`docs/track-b-fork-fix-progress.md:146-152`).

**Pass history (4th-82nd, 2026-09-17/23)**: full narrative in the dated archives ("Docs and tooling
map" below). Condensed current-state trail:

- **43rd-74th (FIXED/REFUTED, live-verified; archives: `_2026-09-22.md`/`_2026-09-23.md`)**: both
  Xvfb SIGSEGVs; D-Bus activation's dropped-CLOEXEC-fd bug; `fd/mod.rs:422` panic; per-fork
  rootfs-rebuild RAM cost (56th); `ssh-agent`/`xfwm4` permanent freeze
  (`RawMutex::WaiterQueue::with_lock`, 60th/61st); `SharedUnixConnectQueue::cancel`'s slot leak
  (62nd); `DBUS_FAILED` root-caused+fixed (a byte-size regression guard discarding a healthy fresher
  writable-layer export, 67th/68th). REFUTED: `/defaults/xfce/` readdir, dbus-daemon babysitter
  SIGKILL, epoll-readiness, GLX/compositor blocker theories. 70th-74th: root-caused two
  logging/capture gaps hiding `xfwm4`'s own X11 traffic (`do_read`'s socket branch is separate from
  `do_recvmsg`; the `unix` closure, not just `net`, needed the `socket_read` diagnostic); added
  low-overhead `litebox_diag::process_timeline`/`socket_read` targets; independently reproduced a
  `GetAllProperties` D-Bus call re-issuing every ~10.7s forever (mechanism unconfirmed, still open).
  `DE_FAILED`/RAM collapse (host process count peaking ~30) survived all of it.
- **75th**: **`xfwm4` launches for the first time ever.** Root-caused+FIXED (`1d449e6`)
  `take_cross_process_writable_layer_export` requiring an env var unset on every `--oci-image`
  boot — the parent never re-absorbed ANY cross-process fork child's filesystem writes, on ANY
  `--oci-image` boot, ever; fixed by mirroring the child's own OCI-image fallback onto the parent's
  read side. Confirmed via `DIAG_TIMELINE execve`, X11 window count 0→1→11,
  `_NET_SUPPORTING_WM_CHECK` advancing. Also REFUTED the rootfs-cache-miss theory
  (`LITEBOX_DIAG_FORK_TIMING=1`: both caches 100% hit, real per-fork rootfs cost ~83-140ms). `DE_UP`
  not reached — RAM crater (0.3-1.3GB free) around `WM_POLL n=6`, 11+ windows.
- **76th**: first DIRECT host-side capture of the crater process tree
  (`.wfgy/pass76_crater_procsnapshot.txt`) — 33 simultaneous processes, 4 fork-tree generations
  deep, ~10.5GB combined working set/15.25GB host. Landed `live_cross_process_fork_children`
  (shared-arena `AtomicU32` admission control, caps 6 concurrent cross-process children, fails open
  ~8s) — real but PARTIAL: slower growth, same eventual magnitude (28-29 processes, 0.17-0.31GB
  free). Reframed: not a scheduling problem — a real XFCE session needs >6 long-lived daemons alive
  simultaneously, each paying ~350MB-1.1GB peak working set once (ordinary Windows behavior); fix
  rate-limits, doesn't cap. Real fix needs lower PEAK PER-PROCESS RSS. Confirmed not a leak (WMI
  `Terminate` fully recovers RAM).
- **77th**: root-caused+FIXED (`621ee1a`) a real 2x host-allocator commit bug —
  `WindowsUserland::alloc` doubled every commit instead of using `VirtualAlloc2`'s unused
  `MEM_ADDRESS_REQUIREMENTS::Alignment` field (`cdb -pv` traced it to `clap`'s CLI-parsing `Vec`
  growth, ~108MB committed before the OCI pull even begins). Verified correct, measured: Priv
  committed 123.7→76.6MB / 130.2→84.5MB single-process, ~35-40% reduction per process. Real but NOT
  sufficient alone: full XFCE boot still cratered at the same magnitude (28 processes, 0.82GB free)
  — 28×~77MB≈2.15GB vs. ~7-8GB actually consumed. New candidate found by code reading, not fixed:
  BOTH fork paths (`Vmem::duplicate`, `litebox/src/mm/linux.rs`~1679-1719; `copy_one_group`,
  `litebox_platform_windows_userland/src/process_fork.rs`) unconditionally byte-copy every
  non-shared VMA on every fork, including read-only shared-library code/rodata a real `fork()`
  would share for free. Deliberately not attempted — flagged as needing real measurement first.
- **78th**: measured the 77th pass's shared-library-COW theory (`LITEBOX_DIAG_FORK_VMA_BREAKDOWN=1`,
  permanent, off by default) instead of guessing — read-only file-backed bytes are only ~29% of
  copied bytes per fork (real but MODERATE, not dominant); declined the skip-copy fix as a bad trade
  against ADVISORY-001 §3N's bug history for a moderate payoff. Incidentally reconfirmed `ls | wc -l`
  can still SIGSEGV/SIGABRT via the known tcache-corruption class.
- **79th**: measured fork-then-immediately-`execve()` instead of guessing — with the `GLIBC_TUNABLES`
  workaround, 20/20 real `bash -c` forks are plain `fork()` (`CLONE_VM` absent) with `execve()` as
  the literal first syscall, ~85% of cycle wall time is eager-copy, 100% wasted at `execve`. Declined
  to implement a skip/defer-copy fix: reaching a "first syscall" checkpoint needs the child to
  execute real instructions first, which needs the copy already done (no COW/page-fault primitive
  exists) — a real fix needs a new per-page lazy-population primitive, scoped but not attempted.
  Also reconfirmed ADVISORY-001 §3N live (44% per-fork crash rate WITHOUT the tunable, 0% with it).
- **80th**: confirmed by direct live measurement (not prediction) that tightening
  `CROSS_PROCESS_FORK_CONCURRENCY_CAP` further is a dead end — two fresh boots at 6→3 hit the SAME
  WM_POLL n=4 crater ceiling the unmodified cap=6 binary already hit (77th), just slower with fewer
  procs alive at the crater instant. Confirms the crater is driven by CUMULATIVE committed memory
  across the boot's whole fork history, not peak instantaneous concurrency — CLOSES admission-cap
  tuning as a lever.
- **81st**: investigated the 79th pass's declined idea in a narrower "defer the WHOLE copy batch (not
  per-page) until the child's first non-`execve` syscall" framing, via code reading only (no code
  changed, no live boot — nothing to verify). Confirms 79th's obstacle (the child needs real,
  populated memory to execute even its first instruction, so there is no window to defer into) and
  finds a SECOND, independent one: the checkpoint is a syscall event, not a memory-WRITE event, and a
  plain `fork()`ed child may legally write memory (stack/TLS/malloc bookkeeping/`atfork` handlers)
  with zero syscalls before `execve` — sharing pages until the checkpoint would let such a write
  silently corrupt the PARENT. Litebox's own `CLONE_VFORK` path already does share-until-`execve`
  safely (`do_clone`, `process.rs:3893-3925`), but only because real `vfork()` carries a POSIX UB
  contract forbidding exactly that write, plus the parent is blocked (`wait_for_vfork_done`) —
  neither property holds for plain `fork()`, which is what the real `bash` workload uses (79th).
  Whole-batch deferral removes the per-page performance argument only, not the correctness one — the
  only remaining viable path is genuine per-page lazy population (79th's own scoped primitive), no
  shortcut exists at either granularity. Re-confirms 79th's decision not to implement.
- **82nd** — both remaining "quick, safe" Track B levers (skip-copying-zero-bytes; trimming optional
  XFCE session-autostart components) tested and confirmed real but genuinely exhausted — neither
  reduces Windows' own `VirtualAlloc2(MEM_COMMIT)` charge, which is what the crater is measured in.
  Net effect: narrowed the whole investigation down to one remaining candidate, genuine per-page
  lazy population. Full Angle A/B evidence, exact XFCE client list, exact before/after process
  counts: archive.
- **83rd-87th (compacted; full narrative: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "83rd-87th pass full
  narrative" section)** — implemented lazy (reserve-then-commit-on-fault) fork memory
  (`litebox_platform_windows_userland/src/lazy_fork_commit.rs`, `LITEBOX_LAZY_FORK_COMMIT=1`), a
  real measured win for the dominant fork-then-`execve` case; found and fixed three real bugs
  along the way (two general cross-process-fork bugs unrelated to laziness itself — a
  guest-mmap/64KiB-alignment-padding collision, and forked children never inheriting the parent's
  `sigreturn_trampoline` address — plus the lazy-specific active-`%rsp`-group bug); found, but did
  NOT fix, Bug 4: a genuine TOCTOU — a lazily-serviced page fault reads the parent's CURRENT
  memory, not a true point-in-time-at-fork snapshot, unsafe for fork-WITHOUT-`execve` (subshells/
  daemons) since the parent can keep mutating its own heap after `fork()` returns. Investigated and
  ruled out two fix candidates (real section-object COW: infeasible without a disruptive allocator
  rewrite; fork-time snapshot: forfeits the dominant case's own win) before converging on and fully
  designing a THIRD: single-generation software COW via guard pages, correctness-sound only for
  exactly one outstanding (fork-time-to-fully-serviced) lazy child per parent at a time — closed
  design gaps: per-parent-process (not tree-wide) claim scope; a double-checked-state read
  protocol closing the design's own TOCTOU; and `VIRTUAL_PROTECT_LOCK`/`fork_verify`-VEH
  coordination, matched against the codebase's own `write_usize_fault_tolerant` precedent. Both
  `LITEBOX_LAZY_FORK_COMMIT` and the not-yet-existing guard-cow flag stayed default OFF throughout;
  `DE_UP` not attempted in any of these passes.
- **88th — IMPLEMENTED the single-generation guard-page COW design, gated behind a SEPARATE,
  additional flag `LITEBOX_LAZY_FORK_GUARD_COW=1` (on top of `LITEBOX_LAZY_FORK_COMMIT=1`), both
  default OFF. Found+FIXED a real, live, guard-cow-specific hang (Bug 5) via an actual boot
  attempt, then re-ran the boot and reached further than any prior pass without cratering RAM.**
  Full mechanism, exact code locations, correctness argument, Bug 5's own root-cause writeup:
  `lazy_fork_commit.rs`'s own module doc comment ("88th pass" section) — this entry is the compact
  summary. Gating: a process-local single-owner slot (`GUARD_COW_OWNER_PID`, CAS-claimed with a
  placeholder plus a bounded `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`+`GetExitCodeProcess`
  liveness reclaim, matching `WindowsUserland::is_process_alive`'s own existing idiom in `lib.rs`)
  — a fork whose parent already has another live, still-outstanding guarded child gets ZERO lazy
  groups at all (forced fully eager for every group, never a mix of guarded and unguarded lazy).
  When the claim succeeds, the parent `VirtualProtect`s its own already-committed pages in each
  lazy group to `PAGE_READONLY` (per-`VirtualQuery`-region) and installs (once, lazily) a
  write-fault VEH that snapshots a page on the PARENT's own first post-fork write to it, publishes
  `state=1` under `VIRTUAL_PROTECT_LOCK` (matching `fork_verify::write_usize_fault_tolerant`'s own
  established locking-from-VEH precedent), and restores the page's real prior protection so the
  write retries and succeeds. The child's lazy fault handler does the double-checked-state read
  exactly as designed: live `ReadProcessMemory` FIRST, re-check the snapshot slot's `state` SECOND,
  prefer the snapshot if it is now set (never the reverse).
  - **Isolated-repro verification (all 5/5, both builds)**: `LITEBOX_PROCESS_FORK=1` alone
    reconfirmed unchanged; fork-then-`execve` clean with real `parent write-fault captured`/
    `snapshot preferred over live read` log lines proving genuine engagement; the subshell
    (fork-without-`execve`) repro — 5/5 killed under `LAZY_FORK_COMMIT=1` alone with the documented
    `malloc.c:2601` Bug 4 signature — clean under both flags; a NEW two-overlapping-forks-one-parent
    repro confirms the exact expected `reserve_group_lazy`/`copy_one_group` call split (first child
    guarded, second forced fully eager, a nested grandchild fork gets its own independent
    process-local slot), correct output from every child.
  - **Bug 5 (found via a REAL boot attempt, not an isolated repro — FIXED, live-verified)**: all
    four isolated repros above passed clean immediately, but attempting the actual
    `de_only_xcensus_seed3.tar` boot (per this task's own item 8) surfaced a real, guard-cow-
    specific hang — root at 150+ CPU-seconds, process tree stuck at exactly 2, zero further
    progress past the second fork; A/B against `LITEBOX_LAZY_FORK_COMMIT=1` alone (stayed low-CPU,
    exited ~30s, hitting Bug 4's already-documented corruption instead — useful independent
    confirmation Bug 4 is real on a genuine multi-fork boot too) proved it was guard-cow-specific.
    Root cause: reclaiming a dead former owner's slot never healed that owner's guard-protected
    regions first — if the parent had simply never gotten around to writing to a page before that
    (short-lived, fork-then-execve) child died, it stayed `PAGE_READONLY`; the next claim's own
    protect walk then re-`VirtualProtect`s the SAME still-protected range, and Win32's `old_protect`
    out-param faithfully reports the CURRENT (already-read-only) state, poisoning the new claim's
    own restore target. The parent's first real write to that page then re-faults on the exact same
    instruction forever — real CPU burned on every exception dispatch, no crash, no progress,
    indistinguishable from outside the process from a hang. **Fix**: reclaiming a dead owner's slot
    now heals every region that claim ever guard-protected (restores each to its OWN recorded
    `old_protect`) BEFORE the new claim's own protect walk can run, using the same lock order
    `guard_cow_write_fault_veh` itself uses. **Verified**: a cheap, targeted 8-sequential-fork
    repro exercising exactly this reclaim shape hung before the fix, completes clean after it
    (`SEQ_DONE`, all forks' own log lines present, zero corruption), both builds; all four original
    repros re-verified 5/5 clean, both builds, unchanged after landing the fix.
  - **Real `de_only_xcensus_seed3.tar` boot result after Bug 5's fix**: ran the full ~195s
    monitoring window WITHOUT cratering and WITHOUT hanging — free RAM held a stable 2.8-4.5GB band
    (9-16 processes) the entire time, qualitatively healthier than every prior pass's own
    documented crater (28-29 processes, <1GB free). Real forward progress reached: `DE_ONLY_START`
    → `XSOCK_WAIT_DONE` → `DBUS_UP` → `DE_LAUNCHED_DIRECT` → `WM_POLL` n=1..12 →
    `XCENSUS_WINDOWS total=1` (a real X window exists) → the SAME already-documented
    `DE_FAILED after 60s` (`_NET_SUPPORTING_WM_CHECK` never appearing — a separate, pre-existing,
    not-yet-root-caused xfwm4 registration gap, unrelated to this mechanism). **`DE_UP` NOT reached
    this pass** — but the presenting blocker at failure was the pre-existing WM-registration gap,
    not RAM/process exhaustion, which is itself the real positive result: for this run, the
    RAM-crater blocker this whole 76th-88th-pass investigation exists to fix was not what stopped
    the boot. One run, not five (each real boot costs ~3+ minutes; landing+documenting Bug 5's fix
    and this one clean data point was judged higher value than more boot repetitions within this
    pass's remaining budget). Both flags stay default OFF pending broader boot verification.
    **Concrete pickup**: re-verify this boot result 2-4 more times for consistency, then root-cause
    the pre-existing `DE_FAILED`/`_NET_SUPPORTING_WM_CHECK` gap now that RAM is no longer in the way
    of reaching it — see priority item 1 below for the exact next steps on that gap.

Fully DONE (kept only as a marker so a future pass doesn't re-attempt): the minimal isolated
cross-process AF_UNIX repro; the `Network` shared-arena redesign's `socket_set`/
`LocalPortAllocator`/`closing_in_background`/`queued_for_closure` slice; DISPLAY/`getenv()` as the
`DE_FAILED` cause; AF_UNIX `connect()` `EAGAIN`-vs-`EINPROGRESS`; `pty_registry`/
`daemon_pty_masters` (`syscalls::pty::SharedPtyTable`, live-verified cross-process); fork's
fd-eligibility scan dropping a redirected 0/1/2 (`raw_fd_is_plain_stdio_device`);
`SharedUnixConnectQueue`'s cancel-on-first-non-blocking-miss gap (`UnixStreamState::Connecting`);
both Xvfb SIGSEGVs.

**Open, in rough priority order:**

1. **`xfwm4` now launches (75th pass, `1d449e6`) — the blocker is no longer filesystem visibility,
   it is pure host-RAM/process-count exhaustion before `DE_UP`.** CLOSED sub-issues:
   `ssh-agent`/`xfwm4` freeze (60th/61st); `DBUS_FAILED`'s regression-guard cause (67th/68th); the
   writable-layer export-path fallback bug (75th). Admission-control (76th) and the fixed
   per-process alloc floor (77th, ~35-40% reduction) are landed, real, partial mitigations.
   Session-autostart trimming and zero-byte-skip (82nd) are closed as dead-end levers. **83rd-87th:
   genuine per-page lazy fork-memory population (`litebox_platform_windows_userland::
   lazy_fork_commit`, `LITEBOX_LAZY_FORK_COMMIT=1`) implemented, a real measured win for
   fork-then-`execve` (the dominant real case); found+fixed three real correctness bugs; found, but
   left OPEN, a fourth (Bug 4: a genuine TOCTOU unsafe for fork-WITHOUT-`execve`, e.g. long-lived
   daemons); ruled out two fix candidates and fully designed a third (single-generation guard-page
   software COW). Full narrative: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "83rd-87th pass" section.**
   **88th pass IMPLEMENTED that design**, gated behind a SEPARATE, additional, default-OFF flag
   (`LITEBOX_LAZY_FORK_GUARD_COW=1`, on top of `LITEBOX_LAZY_FORK_COMMIT=1`) restricting it to
   exactly the case it is provably sound for: at most one outstanding (not-yet-fully-serviced)
   guarded lazy child per parent process at a time. Isolated-repro verification (5/5, both builds,
   4 repro shapes) passed immediately, but a REAL `de_only_xcensus_seed3.tar` boot attempt (this
   task's own item 8) surfaced a real, guard-cow-specific hang (Bug 5: a reclaimed dead-owner slot's
   still-guard-protected pages were never healed before the next claim re-guarded the same range,
   poisoning `VirtualProtect`'s own `old_protect` out-param and causing an infinite same-instruction
   re-fault loop) — root-caused and FIXED same pass, re-verified via both a cheap targeted repro and
   all four original repros, both builds. **With the fix landed, a real boot ran the FULL ~195s
   monitoring window WITHOUT cratering and WITHOUT hanging** (stable 2.8-4.5GB free, 9-16 processes,
   vs. every prior pass's own documented 28-29-process/<1GB crater) and reached real forward
   progress — `DE_LAUNCHED_DIRECT` → `WM_POLL` → `XCENSUS_WINDOWS total=1` (a real window) — before
   hitting the SAME pre-existing, already-documented `DE_FAILED after 60s`
   (`_NET_SUPPORTING_WM_CHECK` never appearing) this file has tracked for many passes as a SEPARATE,
   not-yet-root-caused gap. **`DE_UP` NOT reached this pass, but for the first time the RAM-crater
   blocker this whole 76th-88th investigation exists to fix was not what stopped the boot** — the
   presenting blocker is now the pre-existing WM-registration gap. Full mechanism, Bug 5's own
   root-cause writeup, and the boot's exact log excerpts: `lazy_fork_commit.rs`'s own "88th pass"
   doc section. Both flags stay default OFF pending broader boot re-verification (one run, not
   five, given each real boot costs several minutes).
   **Next pickup, in order**: (1) re-run the `de_only_xcensus_seed3.tar` boot 2-4 more times with
   both flags on to confirm the healthy-RAM result is consistent, not a one-off; (2) if confirmed,
   root-cause the `DE_FAILED`/`_NET_SUPPORTING_WM_CHECK` gap now that RAM headroom makes it
   reachable repeatedly — this file's own standing notes on that gap (item (b) below,
   `XCENSUS_ROOTPROP`/`LITEBOX_DIAG_SOCKET_READ_TARGET=xfwm4`, the ~10.7s `GetAllProperties`
   retrigger) are the concrete starting point; (3) only once that gap is also closed does flipping
   either flag on BY DEFAULT become worth considering, and even then only after the same 5/5
   real-boot rigor this pass's single clean run does not yet meet. `LITEBOX_DIAG_FORK_VMA_
   BREAKDOWN=1` (zero cost when off) remains the permanent tool for measuring any future fix's real
   payoff. `DE_UP` has not been reached by any pass through the 88th; chrome-devtools MCP has been
   `CONNECT_TIMEOUT` every time it was checked (moot until `DE_UP` fires). Lower-priority, still
   open: (a) decompose remaining per-fork cost between rootfs materialization staying resident post
   its cheap (~83-140ms) build vs. Windows loader overhead; (b) use `de_only_xcensus_seed3.tar`'s
   working `/tmp/xcensus.py` (`XCENSUS_SELECTION`/`XCENSUS_ROOTPROP`, real values not `xprop`
   heuristic text) + `LITEBOX_DIAG_SOCKET_READ_TARGET=xfwm4` to check whether the ~10.7s
   `GetAllProperties` retrigger (69th/70th, still unconfirmed) recurs, checking
   `MappingNotify`(34)/XKB at that boundary before the 30th-pass `LD_PRELOAD getenv_probe.so`
   technique (`ps`/`/proc` is blind to cross-process-forked siblings, 65th; `gpg-agent`'s fatal
   `malloc.c:3846` assertion, 52nd, is why the OLD `de_only_seed.tar` dead-ends earlier than
   `_xcensus_seed2/3`); (c) unconfirmed: `LITEBOX_LOG` may not reach forked children's own stderr
   (`process_fork.rs`'s env-block construction possibly drops it) — if so, every prior
   diagnostic-logging conclusion past the FIRST fork generation needs re-weighing; see
   `_2026-09-23.md`.
2. `SharedUnixConnectQueue`'s cancel-on-claim-race slot leak — FIXED 62nd (`unix.rs`); didn't
   resolve item 1's symptom. Other AF_UNIX exhaustion paths still silent (38th, `unix.rs`):
   `SharedUnixAddrPresenceTable` capacity-256 overflow; a key >108 bytes; backlog ignored on
   cross-process accept. Abstract sockets CORRECT.
3. `SafeZoneAllocator`'s `spin::mutex::SpinMutex` still has no dead-holder recovery — lower-urgency
   theoretical risk (the live `ssh-agent`/`xfwm4` freeze once blamed on it was actually `RawMutex`'s
   `WaiterQueue::with_lock`, CLOSED 60th/61st), not tied to any live symptom now.
4. Debugger-root-cause `litebox/src/event/wait.rs:224`'s `unreachable!()` on garbage thread state
   (dozens/boot, most frequent historical panic, NOT yet debugger-confirmed — don't patch blind).
5. `flock_registry`/`drm`/`evdev` (`GlobalState` fields) remain open, same non-POD-payload obstacle
   `SharedPtyTable` is a template for; `timerfd`/`signalfd` are the next-cheapest carriable fd kinds
   before `socket`/`unix-socket`/`epoll`; the writable-layer-visibility gap for LARGE content
   (`/tmp/de.log` etc.) needs its own chunked-publish design, not a widened `SharedFilePublishTable`
   cap. All three lower-urgency, not on the Xvfb/selkies boot path.

## Container images and OCI loading

**`litebox_packager --oci-image <ref> --output <tar>`** pulls, whiteout-merges, rewrites every ELF and
produces a bootable flat tar in one command — supersedes the ad-hoc OCI-pull Python scripts this
project once hand-rolled, retired, do not recreate.

**Runtime in-memory loading** — `--oci-image <ref>` pulls, merges and rewrites every layer in memory;
no host directory is created for the rootfs (a real one hit three Windows-path bugs). Rewritten
layers cache under `.litebox-cache/`, keyed so a rewriter change self-invalidates.
`tar_ro.rs`'s multi-layer index is built ONCE at mount, not per read (was O(entries²), 17.3s →
0.35s fixed) — and, as of the 56th pass, ONCE per boot tree rather than once per fork child too
(`TarRo::live_entries_after_merge`/`from_merged_live_entries`, "Cross-process fork" section below).
Cache internals, the four fixed OOM bugs, tag-verification detail: archive.

Tags verified live, never from the name: `linuxserver/webtop:alpine-mate` ships MATE not XFCE;
`alpine-xfce` doesn't exist; `debian-xfce`/`ubuntu-xfce` ship real XFCE.

**X server choice**: for on-screen DRM/wgpu (`--gui`) use `Xorg` with `modesetting` — litebox's
virtual DRM is legacy-KMS + dumb-buffer + XRGB8888 only (no atomic modeset/GBM/EGL), so a
GBM-first compositor lands on its least-tested fallback and `Xvfb` never touches DRM/KMS at all.
For browser/selkies, `Xvfb` IS correct — its `-shmem` framebuffer works now SysV shm exists.

## A real desktop renders in a browser

**XFCE renders in a real host browser, and MATE too** — full pipeline (Xvfb, selkies/pixelflux
x264, MIT-SHM) inside litebox, reverse proxy host-side only. Working config: selkies
`--addr=0.0.0.0` port **8081**, dashboard over `--publish`, `/websockets` tunnelled to 8081.
Fourteen litebox defects got here, all landed (archive).

**A stock s6-overlay image boots with no flags/stubs**: `/init` runs 16 cross-process children with
zero uncarriable fds. The once-deterministic black XFCE desktop is fixed (runtime rewriter was
corrupting `libLLVM.so.19.1`'s `.dynsym`, mesa `dlopen` failed forever). Rest: archive.

**XFCE also renders on the THREAD-based fork path, gated by one flag** (`docker.io/linuxserver/
webtop:debian-xfce`, `.wfgy/webtop_stack.sh`). Without it, 3/3 boots die ~7s in to ADVISORY-001
§3N's safe-linked-tcache write. Fix: `--env GLIBC_TUNABLES=glibc.malloc.tcache_count=
0:glibc.malloc.mxfast=0` as a GUEST-side `--env` flag (workaround, THREAD-path only).
`LITEBOX_PROCESS_FORK=1` removes that crash class by construction and no longer hits the old
"Fork-after-Xorg" freeze either (35th). A `de_only.sh` boot runs its whole 60s+160s window with
ZERO crash/OOM as of the 57th pass. `DE_FAILED` still fires — NOT "Cannot open display" (refuted,
52nd), NOT RAM exhaustion (57th) — see Track B item 1 for current blockers. Selkies also needs
`--clipboard-enabled=false` on the thread-based path (its clipboard monitor re-triggers the same
corruption every tick) — moot cross-process.

**Open here.** One client per selkies instance, no slot reclaim on reload. A second, distinct
glibc/tcache corruption signature (`double free or corruption (out)` SIGABRT) still sporadically
hits selkies on the THREAD-based fork path under heavy fork load — Track B territory; don't
re-attempt `GLIBC_TUNABLES` without evidence of a third mechanism.

**ACK-stall-kill and port-8081 watchdog — both CLOSED (2026-09-16)**: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

## Host-side crash machinery

A fatal host fault dumps before it dies, ungated (stack walk, `RECENT_FAULTS` ring, `RECOVERY_LOG`,
no env var needed); a real OS minidump comes only from the repeated-identical-fault circuit
breaker. An unexplained `0xC0000005` may be a panic — the VEH handler enters only for the four
codes it triages, registers FIRST in the chain (`docs/veh-exception-handler-design.md`).
Cross-process sync on Windows: every native address/TID-based wait is process-local
(`WaitOnAddress`, keyed events, `NtAlertThreadByThreadId`=ACCESS_DENIED); only a shared kernel
object crosses processes — `RawMutex` (below) is the one that matters; `xproc_sync.rs`'s
named-event primitive is live-verified but still unwired.

## Shared-memory foundations -- all DONE, live-verified 2026-09-16/17/22 (full mechanism: archive)

`RawMutex` no longer calls `WaitOnAddress`/`WakeByAddressSingle` (process-local per MSDN) -- a
manual wait queue + cross-process kernel `Event`s, with `poisoned: AtomicBool` owner-death
recovery. A small 64 MiB `shared_kernel_arena_alloc` backs `SharedArc<T>` for
`LiteBoxX`/`GlobalState` placement (NOT wired to `GlobalAlloc`; `SLAB_ALLOC` stays
private-per-process). **Root cause of the whole `GlobalState`-sharing class**: `SharedArc::new`
shares only `T`'s literal inline bytes -- a `BTreeMap`/similar registry has its NODES on the
private per-process heap, meaningless to an attaching process. Of the original uncarriable-registry
list (`unix_addr_table`/`pty_registry`/`daemon_pty_masters`/`flock_registry`/`fifo_registry`/
`sysv_shm`/`memfds`/`shared_files`): all but `flock_registry` are fixed (per-process-shadowed, a
shared-arena fixed array, or — for `pty_registry`/`daemon_pty_masters` — both a shadow AND a
live-verified cross-process companion, `syscalls::pty::SharedPtyTable`). `sysv_shm` moved to
per-process named-object mapping (51st). Reusable pattern (`SharedUnixAddrPresenceTable`, reused
by AF_UNIX/`SharedPtyTable`): fixed-slot, pure-atomic, lock-free `(kind, key bytes<=108, owner
pid)` side-index. **A mutable-state table on this pattern needs every WRITE path audited for
shared-side mirroring** — `SharedPtyTable`'s own setters originally only reached the local side
(37th-pass live catch). Still open: `flock_registry` (pty's pattern is a template);
`SafeZoneAllocator::alloc`'s spinlock livelock (no dead-holder recovery unlike `RawMutex`).

## Closed — do not re-attempt without a genuinely new approach

VEH_FRAME_STRIDE canary guard, `dev_bench`/`litebox_runner_snp` build failures, CoW-mmap
performance, input-latency bugs, presenter-split duplicate-`SYN_REPORT`, the GUI-protocol
decision, five cheap-wins PRD rows, cross-process-fork stdio-handle bug (`spawn_suspended`'s
clobbered `STARTF_USESTDHANDLES`), presenter-process split (`docs/presenter-process-design.md`)
— all CLOSED, none open. Full detail: archive.

## Docs and tooling map

- **Archives** (newest first) — `_2026-09-23.md` (70th-86th passes, full narrative: the `xfwm4`
  writable-layer-export fix, the live-captured RAM-crater process tree, the 76th-pass
  admission-control fix's honest partial-success evidence, and the full 83rd-85th lazy-fork-commit
  bug-by-bug writeup — all four bugs, exact repro logs, both candidate real fixes for the still-open
  Bug 4), `_2026-09-22.md` (26th-69th passes, full narrative behind every pass-history entry above
  through the 69th), `_2026-09-18.md` (12th-34th), `_2026-09-17.md` (shell-crash, stdio-handle bug),
  `_2026-09-16.md` (Track A audit), `_2026-09-15.md` (ACK-stall-kill), `_2026-09-10.md` (fork fd
  eligibility, OCI cache, s6-boot). Older: `_2026-09-03/05.md`.
- Fork: `docs/track-b-fork-fix-progress.md`, `advisor/ADVISORY-002-d-zero-fork.md`,
  `advisor/ADVISORY-001-fundamentals.md` (§3N tcache). `docs/veh-exception-handler-design.md` —
  read before touching VEH.
- Desktop logs: `docs/webtop-debian-{selkies,xfce}-2026-09-0{6,8}.md`,
  `webtop-xfce-code-vs-data-2026-09-08.md`, `fork-fs-veh-2026-09-08.md`.
- Consult before deriving: `docs/premade-library-research.md`, `docs/drm-dumb-buffer-ioctl-reference.md`,
  `docs/diag-timeline-field-semantics.md` (before any `DIAG_TIMELINE` `comm`-field hypothesis).
- `docs/macos.md` — Apple Silicon guest-execution stub, deferred. NOT implemented:
  `docs/session-daemon-design.md`, `docs/fork-region-grouping-design.md`.
- `advisor/probes/` — diagnostics (`decode_frame.py`, `symbolize_litebox_crash.py`, `dup_probe.c`,
  `drm_flip_probe.c`, `clone_probe.c`, `socketpair_fork_probe.c`, `pty_fork_probe.c`) plus
  `MEASUREMENT-PITFALLS.md`, `DISK-HYGIENE.md`.
- `.gm/memories/` — older per-topic notes, superseded by this file/archives.
