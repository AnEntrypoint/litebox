# litebox — current state (2026-09-23)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, or one a later commit superseded, is deleted rather than hedged. Reference
detail is drained to `docs/AGENTS_ARCHIVE_*.md` and dated `docs/*.md` in the map below — read those
for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one
line plus its pointer, not a separate memory file. Compacted at the 65th, 70th, 72nd, 75th, 76th,
81st, 83rd, 85th, 88th, 91st, 93rd and 95th passes (pass-history section below; 26th-69th full
narrative: `docs/AGENTS_ARCHIVE_2026-09-22.md`; 70th-95th full narrative, including each pass's own
complete evidence and fix rationale: `docs/AGENTS_ARCHIVE_2026-09-23.md`). Re-compacted 95th pass
(drained the 91st-94th passes' own full writeups to the archive now that the 95th pass's own
findings supersede/reconfirm them; same pattern as the 93rd pass's own prior compaction of the 91st
pass alone). Still somewhat over the 30KB target — this file's own "Docs and tooling map"/"Closed"
sections and older CLOSED pass-history entries remain the next drain candidates for a future pass,
once no actively-being-extended entry would be disturbed (the 95th pass's own item-1 entry, the
`GS_BASE`/`NtContinue` `ContextFlags` lead, is exactly such an actively-extended entry — do not
drain it until it is resolved one way or the other).

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
- **95th — reconfirmed the 94th pass's "VEH never invoked" finding on a fresh, independent,
  unperturbed boot (no `cdb`), traced the exact fast-path assembly mechanism that explains WHY, and
  narrowed the root cause from "`FS_BASE` clearing" to "`GS_BASE` clearing, with a chicken-and-egg
  that makes every existing in-VEH repair structurally unable to fire for this fault." `DE_UP` NOT
  reached.**
  - **Live reproduction** (`.wfgy/pass95_diag_run2.combined.log`, `LITEBOX_DIAG_FS_BASE_REPAIR=1
    LITEBOX_DIAG_GS_BASE_REPAIR=1 LITEBOX_LOG=...,litebox_shim_linux::syscalls::process=debug`, the
    94th pass's own recommended config): `xfce4-session` (winpid=22720, confirmed via its own
    `task-resume-probe (child, winpid=22720): built Task ... calling run_thread` line) executes
    exactly ONCE (`grep -c "path=/usr/bin/xfce4-session"` = 1, both hits are the same wrapped log
    line) and is reaped by `wait4_diag` with `exit_code=3221225477` (`STATUS_ACCESS_VIOLATION`) at
    `elapsed_ms_since_thread_start=18549` — same signature, same ~16-19s window as the 91st-94th
    passes. Zero `[diag-fs-base-repair]`/`[diag-gs-base-repair]` lines appear within ~3300 log
    lines of the crash in either direction, while the identical diagnostic fires successfully
    dozens of times elsewhere in the SAME boot for other threads (confirms the repair mechanism
    itself works; confirms it is specifically never reached for this fault) — independent
    reconfirmation of the 94th pass's finding, not a repeat of the same run.
  - **Resolved an apparent "second xfce4-session instance" red herring**: `xfce4-session`'s own
    stdout/stderr is piped through `sed 's/^/[de2] /' ` (`.wfgy/de_only.sh`), which fully-buffers
    when not attached to a terminal — its real startup messages (ConsoleKit warning, ICE authority
    creation) only flush to the combined log file long after the process has already crashed and
    exited, making the log's wall-clock ORDER misleading. `de_only.sh` has no retry/respawn loop
    (`grep -c "xfce4-session"` in that script confirms one launch, backgrounded with `&`, then a
    12x5s `WM_POLL` loop) — there is only ever one `xfce4-session` process. Secondary, unchased
    finding worth flagging: its X11 window (`win=0x200001 ... class='xfce4-session.Xfce4-session'`)
    persists and is still queried by every `WM_POLL` iteration through the full 60s window even
    though the owning process is confirmed dead by `wait4` at 18.5s — `Xvfb` is not cleaning up a
    crashed client's window/connection promptly, a possible separate cleanup-on-abrupt-death gap
    worth a future pass's attention but not investigated further this pass.
  - **Traced `vectored_exception_handler_entry`'s fast-path assembly in full**
    (`litebox_platform_windows_userland/src/lib.rs:428-537`) to understand exactly where a
    `FS_BASE`-cleared repair can and cannot fire: `.Lours` requires (a) `r8` (this thread's
    `TlsState*`, looked up via `gs:[r8*8 + TEB_TLS_SLOTS_OFFSET]` — itself `GS_BASE`-relative) to
    be non-null, (b) `EXCEPTION_ACCESS_VIOLATION`, (c) `rdfsbase() == 0`, (d) a non-null faulting
    `rip`, (e) `tls.guest_fs_base` (the mirror `set_thread_fs_base` maintains) to be non-zero — only
    then does it `wrfsbase`-repair and resume silently (no log line at all, by design). Any miss
    falls to `.Lswap`, the full handler, whose OWN `FS_BASE` repair (`lib.rs:2552-2610`) is what
    `LITEBOX_DIAG_FS_BASE_REPAIR` actually logs. Confirmed by code reading (not live-tested this
    pass) that `install_tls()` (which makes `.Lours`'s own `r8` lookup non-null) always runs, via
    `ThreadHandle::run_with_handle`, strictly BEFORE `ThreadInitState::ForkedChild`'s
    `sys_arch_prctl(SetFs(fs_base))` call ever executes on a freshly spawned thread — so the
    originally-suspected "genuinely uninitialized `THREAD_FS_BASE` shadow" framing (this pass's own
    starting hypothesis, and the 94th pass's item (a)) does not hold up for the normal path: the
    mirror IS correctly populated by the time any guest code runs. `ThreadInitState::NewThread`'s
    `tls: None` case (the other angle this pass was asked to check) is also not implicated — it is
    reached only for a same-process `clone()` WITHOUT `CLONE_SETTLS`, never for `vfork()` (`CLONE_
    VFORK` unconditionally forces `is_process_clone = true` -> `ForkedChild`, per `process.rs:3757`,
    and 3757's own comment; live-confirmed this pass too: `clone: cross-process fork() skipped for
    a vfork child` fires for EVERY vfork, unconditionally, not just on ineligibility).
  - **Checked the 94th pass's angle (b) directly**: `EXTERNAL_GRACE_PERIOD` (`process_fork.rs`, 15s)
    and `SHARED_UNIX_CROSS_CONNECT_TIMEOUT` (`unix.rs:3008`, 15s) are real constants, but neither
    correlates with any wait/timeout event in this pass's own log near the crash -- this specific
    "fixed timeout" mechanism is NOT confirmed. `LIVENESS_CHECK_INTERVAL` (2s) and `MAX_INLINE_
    WAITERS` (32) don't match either. The timing's own tightness across passes remains real and
    unexplained by any single named constant found so far.
  - **Sharpened root-cause hypothesis, the actual headline finding**: since `.Lours`'s own `r8`
    lookup is itself `GS_BASE`-relative, and Windows' own `ntdll` exception dispatcher needs a valid
    `GS_BASE` to locate the TEB's VEH chain before invoking ANY registered handler at all (91st
    pass's own independently-confirmed mechanism) -- every piece of evidence (zero VEH invocation,
    not just zero successful repair) is consistent with `GS_BASE`, not `FS_BASE`, being what's
    genuinely wrong at the fault instant, with `FS_BASE` reading 0 downstream of the same underlying
    event rather than being the primary corruption. This creates a structural deadlock no in-VEH fix
    (FS_BASE OR GS_BASE) can ever close: repairing `GS_BASE` requires the VEH to run, and the VEH
    cannot run without a valid `GS_BASE`. **Ruled out self-inflicted `GS_BASE` writes**: `wrgsbase`
    has exactly ONE call site in the whole crate (`lib.rs:220`, the repair itself) -- litebox never
    writes `GS_BASE` outside that repair, so if it's genuinely getting cleared, the mechanism is
    external (a Windows API side effect) not a direct litebox bug. **Concrete, not-yet-tested next
    candidate found by code reading**: `switch_to_guest_ntcontinue` (`lib.rs:4318-4390`, the path
    EVERY brand-new OS thread's first-ever guest entry MUST take, `ForkedChild` included per
    `has_entered_guest`'s own doc comment) calls `NtContinue` with `ContextFlags: CONTEXT_CONTROL_
    AMD64 | CONTEXT_INTEGER_AMD64` (`lib.rs:4334`) -- notably NOT `CONTEXT_SEGMENTS`. Whether an
    `NtContinue` call that excludes `CONTEXT_SEGMENTS` can still have a real, version/build-
    dependent side effect on the live `SegGs`/`GS_BASE` (Windows kernel exception-return code paths
    are not guaranteed to treat every context field as strictly gated by `ContextFlags`) is unverified
    this pass -- worth a targeted, cheap, off-by-default diagnostic (`rdgsbase()` logged immediately
    before this call, correlated against the first post-resume fault) before either changing the
    flags or ruling this out. **Do not attempt a blind fix to `ContextFlags` without that live
    evidence first** -- this is exactly the class of unverified guess this project's own standing
    discipline (live-evidence-before-fixing) exists to prevent. Two release boots this pass
    (`pass95_diag_run1` died to an unrelated dead-root-runner artifact from a `Start-Job`-based
    launch losing track of its own child process -- host tooling issue, not a litebox bug, fixed by
    launching directly via the tool's own `run_in_background` instead; `pass95_diag_run2` is the
    reproduction above), RAM never observed below ~2.7GB free, no concurrent boots, both cleanly
    terminated via WMI `Terminate`.
Fully DONE (kept only as a marker so a future pass doesn't re-attempt): the minimal isolated
cross-process AF_UNIX repro; the `Network` shared-arena redesign's `socket_set`/
`LocalPortAllocator`/`closing_in_background`/`queued_for_closure` slice; DISPLAY/`getenv()` as the
`DE_FAILED` cause; AF_UNIX `connect()` `EAGAIN`-vs-`EINPROGRESS`; `pty_registry`/
`daemon_pty_masters` (`syscalls::pty::SharedPtyTable`, live-verified cross-process); fork's
fd-eligibility scan dropping a redirected 0/1/2 (`raw_fd_is_plain_stdio_device`);
`SharedUnixConnectQueue`'s cancel-on-first-non-blocking-miss gap (`UnixStreamState::Connecting`);
both Xvfb SIGSEGVs.

**Open, in rough priority order:**

1. **`xfwm4` now launches (75th pass) and the RAM crater is closed (76th-88th, `LITEBOX_LAZY_FORK_
   COMMIT=1`/`LITEBOX_LAZY_FORK_GUARD_COW=1`) — the sole remaining blocker is `xfce4-session` itself
   dying `STATUS_ACCESS_VIOLATION` ~16-19s after its own start.** Full 75th-94th narrative (RAM
   crater fix, `cdb`-captured faulting RIP `0x00007fefe92bc7cb`, the `GS_BASE`/`FS_BASE` repair
   chase and its 4-pass decisive-negative-timing result): `docs/AGENTS_ARCHIVE_2026-09-23.md`'s
   "Item-1 tracker full narrative" section — do not re-read this as a starting point, it is
   superseded by the pass-history entry above. **Current state (95th pass, see pass-history entry
   above for full detail)**: root cause reframed from "`FS_BASE` clearing" to "`GS_BASE` clearing,
   with a structural deadlock — the VEH that would repair it needs `GS_BASE` valid to be invoked at
   all, so it can never fire for this specific fault." Leading untested candidate:
   `switch_to_guest_ntcontinue`'s `NtContinue` call (`litebox_platform_windows_userland/src/
   lib.rs:4318-4390`, the path every brand-new forked/vforked thread's first guest entry MUST take)
   passes `ContextFlags: CONTEXT_CONTROL_AMD64 | CONTEXT_INTEGER_AMD64` — NOT `CONTEXT_SEGMENTS` —
   an unverified but concrete mechanism by which `SegGs`/`GS_BASE` could be affected on exactly this
   thread shape. **Next pickup, precise**: add a cheap, off-by-default `rdgsbase()`-before-`NtContinue`
   diagnostic (no `cdb` needed) to test this live before considering any change to `ContextFlags` —
   do not blind-fix it. If refuted, the `cdb -p` breakpoint on `exception_callback`'s own entry
   (never bare `vectored_exception_handler`, which the 84th pass found is instrumentation-sensitive)
   remains the fallback to see past "VEH never entered" directly.
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
