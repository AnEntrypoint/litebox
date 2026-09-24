# litebox -- current state (2026-09-24)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, or one a later commit superseded, is deleted rather than hedged. Reference
detail is drained to `docs/AGENTS_ARCHIVE_*.md` and dated `docs/*.md` in the map below -- read those
for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one
line plus its pointer, not a separate memory file. Compacted at the 65th, 70th, 72nd, 75th, 76th,
81st, 83rd, 85th, 88th, 91st, 93rd, 95th, 98th and 101st passes (pass-history section below;
26th-69th full narrative: `docs/AGENTS_ARCHIVE_2026-09-22.md`; 70th-100th full narrative, including
each pass's own complete evidence and fix rationale: `docs/AGENTS_ARCHIVE_2026-09-23.md`). 101st
pass drained the 98th-100th passes' own full blow-by-blow (72.8KB -> ~52KB) and this file's own
redundant top-of-file recap of that same material (~52KB -> see below); "Docs and tooling map"/
"Closed" remain the next drain candidates for a future pass still working toward the 30KB target.

**Where things stand, in one paragraph**: cross-process fork (`LITEBOX_PROCESS_FORK=1`) alone is
solid and the default-safe path. The RAM-crater blocker on top of it (`LITEBOX_LAZY_FORK_COMMIT=1
LITEBOX_LAZY_FORK_GUARD_COW=1`) has had six real, independent, live-verified correctness bugs found
and fixed across the 83rd-100th passes (Bugs A/B/3/4/5/6a/6b/7 -- see the "Cross-process fork"
section's pass-history below and the archive for each one's full mechanism); the 101st pass added a
real, verified-safe batched-`VirtualProtect` optimization and, via a real boot, confirmed two things
with hard evidence rather than guesses: (1) that optimization does NOT fix the RAM-crater timing
(real negative result -- the crater is genuine cumulative Windows commit charge across many
concurrent child processes, not guard-cow's own syscall overhead), and (2) a real, still-open
`rc=139` SIGSEGV (matching the OLDER pre-`787b139` Bug-4 signature) is confirmed live on a real
boot, not yet root-caused -- needs a live `cdb` session, not more code reading. **Both lazy-fork
flags remain default OFF. `DE_UP` has not been reached by any of the 101 passes to date.** See
"Cross-process fork"'s own "Open, in rough priority order" item 1 for the precise next pickup.

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
- **75th-82nd (condensed; full narrative for each: `docs/AGENTS_ARCHIVE_2026-09-23.md`)** — 75th:
  **`xfwm4` launches for the first time ever**, root-caused+FIXED (`1d449e6`) a writable-layer
  export-path fallback bug that had silently broken filesystem-write visibility on EVERY
  `--oci-image` boot, ever (RAM crater at `WM_POLL n=6`/11 windows, `DE_UP` not reached). 76th: first
  direct host-side capture of the crater process tree (33 processes/~10.5GB), landed admission
  control (`live_cross_process_fork_children`, caps 6 concurrent) — real but partial (slower growth,
  same eventual 28-29-process/<1GB-free magnitude); reframed as cumulative per-process peak RSS
  across a real XFCE session's >6 necessarily-concurrent daemons, not a scheduling problem. 77th:
  root-caused+FIXED (`621ee1a`) a real 2x host-allocator commit-doubling bug (~35-40%
  per-process reduction, confirmed not sufficient alone) and found, by code reading, fork's own
  unconditional full-VMA-copy (no COW for shared-library pages) as the next candidate. 78th:
  measured that theory instead of guessing — real but moderate (~29% of copied bytes), declined as a
  bad trade against ADVISORY-001's bug history. 79th: measured (not guessed) that the dominant real
  case is fork-then-immediate-`execve()` with ~85% of cycle time wasted on the eager copy; scoped a
  genuine per-page lazy-population primitive as the real fix, correctly declined to implement it
  without a COW/page-fault mechanism in hand. 80th: confirmed by live measurement that tightening the
  admission cap further is a dead end (same crater ceiling, just slower) — closes cap-tuning as a
  lever. 81st: code-reading-only investigation of a narrower deferred-copy variant, found a SECOND
  correctness obstacle (a plain `fork()` child may legally write memory pre-`execve`, unlike
  `vfork()`'s POSIX-guaranteed non-write contract) — re-confirms 79th's decision, no shortcut exists.
  82nd: the two remaining "quick, safe" levers (zero-byte-skip; XFCE autostart trimming) tested and
  confirmed genuinely exhausted — neither touches Windows' own `VirtualAlloc2(MEM_COMMIT)` charge —
  narrowing the whole investigation to the one remaining candidate, genuine per-page lazy population.
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
- **88th-90th (compacted; full narrative: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "88th-90th pass full
  narrative" section)** — 88th: IMPLEMENTED single-generation guard-page COW
  (`LITEBOX_LAZY_FORK_GUARD_COW=1`), found+fixed a real guard-cow-specific hang (Bug 5) via an
  actual boot attempt, then reached further than any prior pass (stable 2.8-4.5GB free, 9-16
  processes, `DE_LAUNCHED_DIRECT` → `WM_POLL` → a real X window) before the same pre-existing
  `DE_FAILED`/`_NET_SUPPORTING_WM_CHECK` gap. 89th: reconfirmed the RAM fix across 2 more boots;
  root-caused `DE_FAILED` to a REAL, live `xfce4-session` process crash
  (`exit_code=3221225477`=`STATUS_ACCESS_VIOLATION`, ~16-31s after thread start) on the OLD
  thread-based-fork path, unrelated to lazy/guard-cow; first live `cdb` attach on a cross-process
  fork child achieved but inconclusive (sustained attach perturbs the target's own timing). 90th:
  root-caused the crashing fork to a genuine `CLONE_VFORK` and fixed two real architectural bugs
  this uncovered (a vfork child's blind fresh `PageManager` risking silent live-parent-memory
  corruption on `execve`; a regression that fix itself caused in `release_memory`/`Vmem::duplicate`
  sweeping up the parent's own memory) — both live-verified (`Xvfb` no longer crashes; `xfce4-session`
  reaches real GTK/ICE startup). **The original `xfce4-session` crash itself — `STATUS_ACCESS_
  VIOLATION` ~16-16.7s after its own start, immediately after its own `vfork()` of `/bin/sh`, with
  ZERO `[veh]`/panic output anywhere in the log — survived both fixes unchanged, still OPEN going
  into the 91st pass.**
- **91st-94th (compacted 95th pass; full narrative `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "91st-94th
  pass full narrative" section)** — chased a "Windows clears `GS_BASE`/`FS_BASE` under scheduling
  pressure" theory across four passes: 91st found and fixed a real `GS_BASE` repair gap in
  `RawMutex::block_or_maybe_timeout`'s wait loop; 92nd disproved that specific fix live (zero
  repairs fired, identical crash) and found the exact deterministic faulting guest RIP
  (`0x00007fefe92bc7cb`) via the Windows Application Error event log; 93rd live-captured it with
  `cdb -p` for the first time (guest glibc `__syscall_error`'s `mov fs:[rcx], eax` errno store,
  `FS_BASE` reading 0) and added the matching `FS_BASE` repair, live-verified NOT sufficient; 94th
  added `LITEBOX_DIAG_FS_BASE_REPAIR=1` (mirrors `LITEBOX_DIAG_GS_BASE_REPAIR`), found+fixed two
  more real repair gaps (`finish_real_timeout`, `poll_until_value_changes`), and reached the
  decisive negative result that anchors the 95th pass below: across 4 different repair-coverage
  configurations the crash's timing stayed within ~550ms — too tight for a genuine scheduling race
  — and NONE of the repair diagnostics, not even the pre-existing unconditional VEH catch-all
  prints, ever fire near the crash, meaning this codebase's own `vectored_exception_handler` is
  never even being INVOKED for the fault that kills `xfce4-session`. All `RawMutex` repair fixes
  from these four passes are real, general, and stay landed regardless.
- **95th-97th (compacted 98th pass; full narrative: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "95th-97th
  pass full narrative" section) — a 6-pass misattribution (91st-97th), resolved.** 95th traced
  `vectored_exception_handler_entry`'s fast-path assembly and sharpened "FS_BASE clearing" (91st-94th's
  theory) to "GS_BASE clearing, with a chicken-and-egg no in-VEH fix can close" (repairing GS_BASE
  needs the VEH to run; the VEH can't run without a valid GS_BASE). 96th REFUTED a `CONTEXT_SEGMENTS`
  fix idea by hard fact (AMD64 `CONTEXT` has no `FsBase`/`GsBase` field) and instead fixed the real gap
  the same reasoning surfaced: `switch_to_guest` never repaired `GS_BASE` immediately before resume the
  way it already did for `FS_BASE` — live-verified 3/3: the silent whole-host-process death is gone,
  replaced by a contained per-thread crash (`xfce4-session`'s vfork'd `/bin/sh`, believed at the time).
  **97th overturned that belief**: the real, reproducible crash is a plain `CLONE_THREAD` pthread
  (`comm=gdbus`, GLib's own D-Bus worker) hit by the ALREADY-DOCUMENTED `lazy_fork_commit.rs` Bug 4
  TOCTOU — confirmed via a clean live A/B (removing the inherited-but-unsafe `LITEBOX_LAZY_FORK_COMMIT=1
  LITEBOX_LAZY_FORK_GUARD_COW=1` env vars: 0/1 `gdbus` crashes vs. 2/2 with them, and `xfce4-session`
  reached `xfwm4` for the first time) — meaning the 91st-96th passes' entire FS_BASE/GS_BASE chase,
  while each fix landed is real and general, was chasing a MISATTRIBUTED symptom of Bug 4, not its own
  independent bug. 97th also fixed a real, separate, valuable bug: `VEH_FRAME_STRIDE`/`EXCEPTION_
  RECORD_RESERVE` were sized only for release codegen, crashing every debug-build boot in ~3s via the
  project's own overflow canary — fixed with an 8x widening gated `#[cfg(debug_assertions)]`, release
  unchanged; debug-build live debugging is viable for the first time as a result. Net effect entering
  the 98th pass: the real remaining blocker is precisely Bug 4 itself (below), not a register-corruption
  mystery — see the 98th pass's own entry for the fix.
- **98th-100th (compacted 101st pass; full narrative: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s
  "98th-100th pass full narrative" section).** 98th generalized Bug 4's fix from one outstanding
  lazy-fork child per parent to any number of concurrent generations (`GUARD_PAGE_REGISTRY`, a
  process-local `Mutex<HashMap<page, PageGuardEntry>>` replacing the single-claim `GUARD_STATE`),
  re-deriving the correctness unit as one shared open interval per page rather than per-generation
  shadow versions; found+fixed a real infinite same-page re-fault livelock of its own (a page left
  `PAGE_READONLY`-poisoned when its last pending generation died without healing) before landing,
  5/5 clean both repros both builds. A single uncontrolled real-boot data point read as promising
  (`DE_LAUNCHED_DIRECT` + real D-Bus traffic before cratering). **99th turned that into a controlled
  3-run A/B and found it was NOT unlucky load: `787b139` is a real, 100%-reproducible regression on
  BOTH axes** — 3/3 craters at 20-21.3s/7 processes (vs. 88th/89th's 195-300s/9-16-proc baseline)
  AND a new `XCENSUS_PRE_DE rc=134` heap-corruption signature in a plain fork-then-execve `python3`
  call, absent in the immediately-prior clean 96th/97th-pass logs. No blind fix attempted — code
  read as internally consistent in isolation, so the defect needed a real many-sequential-prior-
  forks shape none of the isolated repros exercised. **100th root-caused `rc=134` to Bug 7**
  (`sys_execve` reloads a guest image in place without ever calling `lazy_fork_commit`'s
  `disarm_on_execve` — a process that was EVER a lazy-fork child keeps servicing faults against its
  ORIGINAL parent across arbitrarily many FUTURE unrelated `execve()`s, exactly the real
  `xrdb`→`sh`→`cpp`→`cc1` chain on the boot), fixed via a new `DISARMED_BY_EXECVE` flag checked
  first in `lazy_commit_veh`, hooked from `WindowsUserland::end_fork_child_verification`. Also
  fixed two real bugs found by code reading: Bug 6a (`guard_one_page` opened a fresh `OpenProcess`
  handle per PAGE instead of once per claim — O(pages) handle churn on a large guarded group; fixed
  with a shared `Arc<SharedChildHandle>` per claim) and Bug 6b (`GUARD_PAGE_REGISTRY` had no hook
  into ordinary guest `mprotect()`/`munmap()`, letting them silently desync the registry from real
  Windows page state — fixed via a new `invalidate_guarded_range`, live-fired 33 times in a
  targeted repro). **All three fixes verified live-real-boot, before/after**: the specific `rc=134`
  corruption is confirmed GONE post-fix (2/2), but a DIFFERENT, OLDER signature resurfaced —
  `rc=139` plain `SIGSEGV`, 2/2 — matching the PRE-`787b139` Bug-4 signature the 88th pass's
  ORIGINAL single-generation code used to close cleanly (clean `rc=0` in the 96th/97th-pass logs
  under that code); the multi-generation registry doesn't close this case as reliably, reason not
  found this pass. Crater speed (~14-19s/7 procs) is UNCHANGED by all three fixes — leading
  (unverified) suspect flagged: `guard_one_page`'s page-at-a-time, not per-range, `VirtualProtect`
  calls. Both flags stay default OFF throughout 98th-100th; `DE_UP` not reached by any of them.
- **101st pass — implemented and live-verified the batched-`VirtualProtect` optimization the
  89th/100th passes' own doc comments scoped (Angle 2); confirmed by a REAL boot A/B that it does
  NOT fix the crater-speed regression (real negative evidence, not guessed); confirmed by the SAME
  real boot that the 100th pass's `rc=139` SIGSEGV (Angle 1) is genuinely reproducible (6x
  `status=139`/28x `signal=11` events across many distinct commands within the first ~15s) but did
  NOT root-cause it — that needs a live `cdb` session this pass's tool access could not safely
  provide. Both flags remain default OFF; `DE_UP` NOT reached.**
  - **Angle 2 (implemented)**: [`try_guard_region_batched`] (`lazy_fork_commit.rs`), called first by
    `reserve_group_lazy_guarded`'s inner loop — when an entire `VirtualQuery`-uniform sub-range has
    zero existing `GUARD_PAGE_REGISTRY` entries (checked under one lock acquisition), guards it with
    ONE `VirtualProtect` call instead of one per page (confirmed live: `batched region ... (33
    pages) opened fresh in one VirtualProtect` on both the isolated repro and the real boot).
    Falls back to the original, unmodified per-page `guard_one_page` loop the instant any page in
    the sub-range already has a live entry, so the join-vs-open correctness argument is untouched —
    this is a strict subset of what the per-page "open fresh interval" branch already did, applied
    via fewer syscalls. Verified: fork-then-execve and fork-without-execve subshell repros clean
    (1/1 and 3/3 respectively, debug build; the batched log line fires on both), `LITEBOX_PROCESS_
    FORK=1` alone (both lazy flags unset) reconfirmed to emit zero `lazy_fork_commit` log lines —
    byte-identical default path. **Real boot A/B** (`de_only_xcensus_seed3.tar`, fresh release
    build, `.wfgy/pass101_realboot_run1.*`): crater at 15.4s/7 processes, free RAM 7.56GB→0.84GB —
    statistically the SAME shape as the 99th/100th passes' pre-batching baseline (14-21s/7 procs).
    **Real, valuable negative result**: batching the parent-side `VirtualProtect`/lock-acquire count
    does not move the crater's timing or process-count ceiling, meaning the crater is not driven by
    guard-cow's own syscall/VAD-fragmentation overhead — consistent with the standing framing that
    it is genuine cumulative `VirtualAlloc2(MEM_COMMIT)` charge across many concurrent child
    processes' own working sets, a cost this fix (or any guard-cow-internal change) cannot touch.
    Kept landed regardless — real, verified-safe reduction in per-fork syscall count with zero
    measured correctness regression, independent of whether it helps the crater.
  - **Angle 1 (confirmed real, not root-caused)**: the same real boot's `stderr` shows 6 `DIAG_
    TIMELINE exit_group ... status=139` lines and 28 `signal=Signal(11)` lines across many distinct
    short-lived commands (`mkdir`, `cat`, `xset`, `python3`, `xrdb`, `ls`, `sleep`, plus a `sh`
    sub-chain) within the first ~15s, most crashing within ~1ms of their own `execve` — real,
    reproducible, broad, not narrow to one command. Code-reading comparison of the old
    single-generation `guard_cow_write_fault_veh`/heal path against the new per-page
    `guard_one_page` registry (`git show 787b139^:...lazy_fork_commit.rs` vs. HEAD) did not surface
    an obvious correctness gap — both the prune-then-heal sequencing and the double-checked-state
    child read look sound on inspection; the defect most likely needs a live `cdb -p` attach
    (debug build) on the exact real-boot fault to localize, which this pass's tool access does not
    safely support. **Do not attempt to root-cause this by further code reading alone next pass** —
    escalate straight to a live debugger session per the project's own standing practice for this
    bug class. Logs: `.wfgy/pass101_execve.log`, `.wfgy/pass101_subshell{,_1,_2}.log`,
    `.wfgy/pass101_baseline.log`, `.wfgy/pass101_realboot_run1.{out,err,poll}.log`.
- **102nd — root-caused and FIXED the crater-speed regression (Issue 1/Angle A) by direct code
  reading, then verified the fix with 2/2 real boots; Angle 2 (`rc=139`) confirmed still real and
  unaffected, root cause still open.**
  - **Root cause (code-grounded, not guessed)**: `git show 787b139^:...lazy_fork_commit.rs` (old,
    single-owner-gated code) vs HEAD shows every successful guard-cow claim, OLD and NEW alike,
    `Box::leak`s a `Vec<GuardSnapshotSlot>` table sized ONE FULL PAGE (`AtomicU8` + `[u8; PAGE_SIZE]`)
    per guarded guest page — i.e. a claim guarding an 8 MiB heap/stack group eagerly commits ~8 MiB
    in the PARENT at claim time, unconditionally, regardless of whether the child ever touches a
    single one of those pages before `execve`/exit discards the mapping (already documented,
    accepted, in the 88th/98th passes' own doc comments as "the accepted leak-per-successful-
    guarded-fork tradeoff"). The 88th pass's single-owner `GUARD_STATE` gate had an UNDOCUMENTED
    second effect nobody had named: any fork temporally overlapping an already-open claim fell back
    to fully EAGER (`try_claim_guard_cow_for_fork`'s decline path), meaning NO table was ever
    allocated for that fork — during a real boot's fork storm (lots of temporal overlap from busy
    parents), most overlapping forks paid zero extra parent-side commit. The 98th pass's
    generalization to unboundedly many concurrent claims (bounded only by the UNRELATED
    `live_cross_process_fork_children` cap, 6, which bounds concurrent CHILDREN system-wide, not
    concurrent open guard-cow GENERATIONS per parent) removed that incidental rate limit: every
    lazy-eligible fork now succeeds in claiming lazily and leaks its own full-size table, so a busy
    parent can now accumulate up to 6x as much simultaneously-live leaked commit as the old code
    ever could — a direct, sufficient explanation for "craters faster with FEWER processes" that
    needs no new theory about guard-cow's own syscall overhead (consistent with, not contradicting,
    the 101st pass's own real negative result ruling that out).
  - **Fix (`litebox_platform_windows_userland/src/lazy_fork_commit.rs`)**: new
    `GUARD_COW_OPEN_CLAIMS: AtomicU32` + `GUARD_COW_CONCURRENT_CLAIM_CAP: u32 = 3` restore the old
    gate's rate-limiting side effect, generalized to N>1 instead of hardcoding N=1 (which would
    reintroduce the exact TOCTOU the 98th pass fixed for genuinely concurrent multi-child-from-one-
    parent workloads). `try_claim_guard_cow_table` now declines (CAS loop, falls the whole fork back
    to eager, exactly like the pre-98th-pass gate did for ANY overlap) once the cap is reached.
    Admission is released via the EXISTING `SharedChildHandle::drop` (fires once per claim, exactly
    when the last page relying on that claim's snapshot data has been serviced/pruned/aborted — the
    real end of a claim's memory-cost lifetime, not the much-shorter-lived `GuardCowClaim` struct's
    own drop), plus a new `impl Drop for GuardCowClaim` covering the one edge case
    `SharedChildHandle` can't (a claim admitted but that ends up guarding zero pages, e.g. its
    child's own memory reservation failed before any page ever joined — `child_handle` stays `None`
    for its whole life). Cap value (3) is a reasoned default (small enough to meaningfully bound the
    regression, large enough to still exercise the 98th pass's own multi-generation correctness fix)
    — NOT yet empirically tuned against multiple cap values on a real boot; flagged as a follow-up.
  - **Isolated-repro verification (debug build, both flags default OFF unaffected)**: fork-then-
    execve (`bash -c 'echo hello; sleep 0.2; echo done'`) and fork-without-execve subshell both clean
    with flags unset, byte-identical to pre-102nd-pass behavior. With both lazy flags ON, a NEW
    8-concurrent-subshell repro (`for i in 1 2 3 4 5 6 7 8; do ( x=child_$i; echo $x ) & done; wait`)
    with `LITEBOX_DIAG_LAZY_FORK_COMMIT=1` shows the cap firing exactly as designed — 5 real
    `guard-cow claim DECLINED (cap)` lines across 8 concurrent forks (3 admitted lazily, 5 fall back
    to eager) — with ALL EIGHT children printing their own correct value, 2/2 runs, no
    cross-contamination (the exact correctness property the 98th pass's generalization exists to
    protect, confirmed still intact under the new cap).
  - **Real boot A/B, `de_only_xcensus_seed3.tar`, release build, 2/2 runs — the actual fix
    verification**: both runs dip to 7-8 processes / ~2.3-2.6GB free around t=20-35s (same magnitude
    the 99th-101st passes' UNFIXED code also reached), then, unlike the unfixed code's crater-to-
    kill-switch at 14-21s, RAM RECOVERS and STABILIZES at 5 processes / ~3.6-4.2GB free for the rest
    of the run, with the root process exiting cleanly on its own at t=188.0s (run 2) / t=188.1s (run
    1) — not RAM-killed, not crashed. This is a direct measured fix of the regression: from a
    14-21s/7-process crater back to a stable ~188s window, in the same order of magnitude as the
    pre-98th-pass code's own documented 195-300s/9-16-process stable baseline. Both runs reach
    `DE_LAUNCHED_DIRECT`, run `WM_POLL`/`XCENSUS` cycles up to `n=12` with `XCENSUS` returning clean
    `rc=0` every single time (2/2), before `DE_FAILED after 60s` fires (the window-manager-completion
    blocker is UNCHANGED, pre-existing, not something this fix touches or claims to fix). Logs:
    `.wfgy/pass102_realboot_run{1,2}.{out,err,poll}.log`, `.wfgy/pass102_concurrent{,_run2}.log`,
    `.wfgy/pass102_baseline_{execve,subshell}.log`, `.wfgy/pass102_lazy_execve.log`.
  - **Angle B (`rc=139`) — confirmed still real, unaffected by this fix, root cause still open.**
    Both real boots show 44 `rc=139`/`Signal(11)` events each (same order of magnitude as the 101st
    pass's own 6 `status=139`+28 `signal=Signal(11)` count), including a live
    `DBUS_XFCONF_PROBE rc=139` in both runs — genuinely reproducible even in the plain isolated
    fork-then-execve+guard-cow repro (`.wfgy/pass102_lazy_execve.log`: `winpid=8916` crashes with
    `signal=Signal(11)` essentially immediately after `task-resume-probe ... calling run_thread`,
    `comm` still all-zero, i.e. before the crashing task ever got far enough to record its own name)
    — a NEW, smaller, more isolated repro of this bug class than the 101st pass had, worth chasing
    with the 101st pass's own recommended live-`cdb`-attach approach next, rather than the full real
    boot. This pass did not attempt that live debugging session (time-boxed to Angle A given the
    real, verified win available there) — root cause remains unknown, `LITEBOX_LAZY_FORK_GUARD_COW`
    stays unsafe to recommend as a default until it's fixed. Critically: `rc=139` does NOT itself
    block a boot from proceeding — both real boot runs kept advancing (`WM_POLL`/`XCENSUS` cycling
    successfully) despite ~44 of these events each, so this is a correctness bug, not (by itself) a
    boot-blocking one; `DE_FAILED` is a separate, still-open, pre-existing issue.
  - **Both lazy flags remain default OFF** — this fix only changes behavior when
    `LITEBOX_LAZY_FORK_COMMIT=1`/`LITEBOX_LAZY_FORK_GUARD_COW=1` are explicitly set; confirmed via
    the flags-unset isolated repros above. Not yet safe to flip on for a real boot recommendation:
    Angle B's `rc=139` remains open and, per the project's own standing caution, a real XFCE session
    forks many more long-lived daemons than any isolated repro exercises.
Fully DONE (kept only as a marker so a future pass doesn't re-attempt): the minimal isolated
cross-process AF_UNIX repro; the `Network` shared-arena redesign's `socket_set`/
`LocalPortAllocator`/`closing_in_background`/`queued_for_closure` slice; DISPLAY/`getenv()` as the
`DE_FAILED` cause; AF_UNIX `connect()` `EAGAIN`-vs-`EINPROGRESS`; `pty_registry`/
`daemon_pty_masters` (`syscalls::pty::SharedPtyTable`, live-verified cross-process); fork's
fd-eligibility scan dropping a redirected 0/1/2 (`raw_fd_is_plain_stdio_device`);
`SharedUnixConnectQueue`'s cancel-on-first-non-blocking-miss gap (`UnixStreamState::Connecting`);
both Xvfb SIGSEGVs.

**Open, in rough priority order:**

1. **`xfwm4` launches and survives (75th, confirmed 97th); Bug 4's TOCTOU has a general
   multi-generation fix landed (98th); that fix's own `rc=134` regression is root-caused+FIXED
   (100th, Bug 7) alongside two other real bugs (Bug 6a/6b); the crater-speed regression is
   root-caused+FIXED (102nd, `GUARD_COW_CONCURRENT_CLAIM_CAP`) and verified 2/2 on a real boot —
   stable ~188s runs at 5 processes / ~3.6-4.2GB free instead of the 99th-101st passes' 14-21s
   crater at 7 processes; `rc=139` SIGSEGV is confirmed still live (both the real boot, 44
   events/run 2/2, AND now a NEW minimal isolated repro, 102nd) but STILL not root-caused — it does
   NOT block boot progress by itself. `DE_UP` has not been reached by any pass; both real boots
   reach `DE_LAUNCHED_DIRECT` and cycle `WM_POLL`/`XCENSUS` (clean `rc=0` every time) before
   `DE_FAILED after 60s`, a separate, pre-existing, still-open blocker this pass did not
   investigate.** Full evidence for 98th-102nd: pass-history section above (102nd entry) and
   `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "98th-100th pass full narrative". **Next pickup, precise**:
   (a) empirically tune `GUARD_COW_CONCURRENT_CLAIM_CAP` (currently 3, a reasoned but unmeasured
   default) against real boot A/B at a few different values, extending
   `LITEBOX_DIAG_FORK_VMA_BREAKDOWN=1` to report `GUARD_COW_OPEN_CLAIMS`'s own high-water mark if
   useful; (b) a live `cdb -p` attach (debug build; invasive `-p`, not `-pv`, which cannot receive
   debug events per the 93rd pass; `sxd av` measurably induces its own exception-dispatch livelock
   on this codebase per the 97th pass -- prefer the existing `LITEBOX_DIAG_LAZY_FORK_COMMIT=1`
   diagnostics first) on `rc=139`, now reproducible via the much smaller/cheaper 102nd-pass isolated
   repro (`.wfgy/pass102_lazy_execve.log`'s `winpid=8916` crash) instead of only the full real boot;
   (c) once `rc=139` is fixed and re-verified, attempt the actual `DE_UP`/`DE_FAILED` blocker itself,
   which no pass through the 102nd has yet investigated in its own right. Do NOT retune
   `live_cross_process_fork_children`'s admission cap in response to `rc=139` -- that signature is a
   correctness bug a capacity lever cannot fix and could mask (`GUARD_COW_CONCURRENT_CLAIM_CAP` is a
   DIFFERENT, new, narrowly-scoped cap that only bounds guard-cow's own parent-side commit cost).
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

- **Archives** (newest first) — `_2026-09-23.md` (70th-97th passes, full narrative: the `xfwm4`
  writable-layer-export fix, the live-captured RAM-crater process tree, the 76th-pass
  admission-control fix's honest partial-success evidence, the full 83rd-88th lazy-fork-commit
  bug-by-bug writeup (all five bugs, exact repro logs, the single-generation guard-cow design and
  implementation), and the 91st-97th passes' full FS_BASE/GS_BASE register-corruption chase (real,
  landed fixes, ultimately a misattributed symptom of Bug 4 — see this file's own 98th-pass entry
  for Bug 4's actual fix), `_2026-09-22.md` (26th-69th passes, full narrative behind every pass-history entry above
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
