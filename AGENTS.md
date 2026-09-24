# litebox — current state (2026-09-24)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, or one a later commit superseded, is deleted rather than hedged. Reference
detail is drained to `docs/AGENTS_ARCHIVE_*.md` and dated `docs/*.md` in the map below — read those
for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one
line plus its pointer, not a separate memory file. Compacted at the 65th, 70th, 72nd, 75th, 76th,
81st, 83rd, 85th, 88th, 91st, 93rd, 95th and 98th passes (pass-history section below; 26th-69th
full narrative: `docs/AGENTS_ARCHIVE_2026-09-22.md`; 70th-97th full narrative, including each
pass's own complete evidence and fix rationale: `docs/AGENTS_ARCHIVE_2026-09-23.md`). 98th pass
drained the 95th-97th passes' own full blow-by-blow (64.2KB -> 45.5KB) now that their conclusion —
the 91st-96th register-corruption chase was a misattributed symptom of Bug 4, not an independent
bug — is superseded by Bug 4's own fix landing the same pass; the "Docs and tooling map"/"Closed"
sections remain the next drain candidates for a future pass still working toward the 30KB target.

**98th pass, the headline result: Bug 4 (the guard-cow TOCTOU that was the 91st-97th passes' real,
misattributed blocker) is FIXED in general form** — generalized from one outstanding lazy-fork
child per parent (88th pass) to any number of concurrent generations, by re-deriving the
correctness unit (one shared open interval per page, not per-generation shadow versioning) rather
than implementing the originally-sketched design literally. Verified 5/5 clean on both original
repros (fork-then-execve; fork-without-execve subshell) plus a NEW 3-concurrent-subshell repro,
both debug and release. **Found and fixed a real, 100%-reproducible infinite-livelock bug of its
own during verification** (a page left `PAGE_READONLY`-poisoned when its last pending generation
died without ever restoring it — the same shape as the 88th pass's own Bug 5, reintroduced via a
different trigger) — caught by a purpose-built repro and a targeted diagnostic, not shipped blind.
A real boot attempt (both flags on) reached `DE_LAUNCHED_DIRECT` and real D-Bus traffic before the
RAM crater hit hard within 5 seconds — `DE_UP` still not reached, and the 98th pass read this as
"purely the original RAM-crater problem again, not a correctness one" since it was a single,
uncontrolled data point.

**99th pass turned that one data point into a controlled 3-run comparison and found the 98th
pass's own optimistic reading does NOT hold — the multi-generation guard-cow rewrite (`787b139`) is
itself a real, 100%-reproducible correctness regression, not just unlucky host load.** 3/3 identical
`de_only_xcensus_seed3.tar` boots (rebuilt release binary confirmed current with `787b139`, same
harness/env as the 96th-97th passes' own scripts) crater to <1GB free in **~20-21s**, at only
**7 processes** — a severe regression from the 88th/89th passes' own measured 2.8-4.6GB free /
9-16 processes sustained for 195-300s under the PRE-98th (single-generation) guard-cow code. All
3 runs also show a NEW, identically-reproducible heap corruption: `XCENSUS_PRE_DE rc=134
corrupted size vs. prev_size` (a glibc SIGABRT in `python3 /tmp/xcensus.py`, a plain
fork-then-execve — exactly the "dominant, verified-safe" case the 98th pass's own isolated repros
claimed 5/5 clean) — absent (`rc=0`, clean) in the immediately-prior `pass96_boot{1,2,3}`/
`pass97_releaseboot1` logs, which ran the SAME env-var configuration against the PRE-98th code.
See the 99th pass-history entry and item 1 for the full numbers, the isolation attempt, and the
precise next pickup — this is a real correctness bug in `787b139`, not a RAM-crater capacity
question, so admission-cap retuning is explicitly NOT the right lever here.

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
- **98th pass — IMPLEMENTED and landed the general fix for Bug 4 (the guard-cow TOCTOU), generalizing
  the 88th pass's single-outstanding-child mechanism to any number of concurrent generations per
  parent. FOUND AND FIXED a real, reproducible infinite-livelock bug of its own during verification
  before landing. Both flags stay default OFF; `LITEBOX_PROCESS_FORK=1` alone is unchanged.**
  `litebox_platform_windows_userland/src/lazy_fork_commit.rs`'s own doc comment ("89th pass" section,
  its internal numbering, one behind this file's 98th) carries the full design derivation — read it
  before touching this mechanism again.
  - **The key simplification** (re-derived, not merely widened, from the 87th/88th passes' own "N
    tagged shadow slots" sketch): a page's `PAGE_READONLY` guard means no write has landed since it
    was guarded, so EVERY generation that forks while a page is already open is, by construction,
    relying on the identical live value — one shared "open interval, growing/shrinking set of pending
    generations" per page is correct, not merely convenient; no per-generation shadow versioning is
    needed at all. Replaced the single owner-pid gate + one-claim-at-a-time `GUARD_STATE` with
    [`GUARD_PAGE_REGISTRY`] (`Mutex<Option<HashMap<usize, PageGuardEntry>>>`, process-local — the
    correctness unit is per-PARENT-PROCESS, per the 87th pass's own still-valid finding 1, so no
    shared-arena/`SharedArc` structure is needed). [`try_claim_guard_cow_table`] no longer declines
    based on "another child outstanding" — concurrency is bounded for free by the pre-existing
    `live_cross_process_fork_children` admission cap (6, 76th pass). Liveness for a page's pending
    generations is tracked via a kept-open `HANDLE` per generation (immune to PID reuse over a
    page's whole open interval), not a re-resolved pid.
  - **Bug found DURING verification, not shipped blind: a real, 100%-reproducible infinite same-page
    re-fault livelock (100+ CPU-seconds, zero forward progress) in the first draft's own dead-
    generation pruning.** Root cause: when pruning discovered a page's last pending generation had
    died, the entry was dropped from the map WITHOUT restoring the page's real Windows protection —
    the page stayed `PAGE_READONLY` from the dead generation's own never-triggered guard. The NEXT
    generation's own fresh-guard `VirtualProtect` call then captured the CURRENT (already-
    `PAGE_READONLY`) value as if it were the true original — the exact "poisoned `old_protect`"
    shape the 88th pass's own Bug 5 already named, reintroduced via a different trigger (pruning
    finding zero survivors, not a claim-level reclaim). Found via a NEW purpose-built repro (3
    overlapping `(...)&` fork-without-execve subshells from one parent, each preceded by a real
    100KB heap-mutating `$(...)` command substitution) and a targeted diagnostic
    (`LITEBOX_DIAG_LAZY_FORK_COMMIT=1`'s existing gate, extended with a same-page-repeat counter) —
    not guessed. **Fix**: `guard_one_page` now heals (restores `true_original_protect`, removes the
    entry) ANY page found with an empty `pending` list, whatever the cause (just-pruned-to-empty, or
    born empty from the "child died between spawn and guard" branch), before ever deciding
    fresh-guard vs. join. Confirmed via the same repro: 83 healing events, all correct
    `true_original_protect` values, zero re-fault loops, 5/5 clean.
  - **Verification, all real, both debug and release**: fork-then-execve repro (5/5 both builds);
    fork-without-execve subshell repro (5/5 both builds); the new 3-concurrent-subshell repro (5/5
    debug, 3/3 release, mechanism genuinely engaging — hundreds of open/join/capture/heal log lines
    per run); `LITEBOX_PROCESS_FORK=1` alone, both new flags unset, reconfirmed byte-identical
    (zero `lazy_fork_commit` log lines, correct output) both repro shapes.
  - **Real boot attempt** (`de_only_xcensus_seed3.tar` via `linuxserver/webtop:debian-xfce`, both
    flags on): reached `DE_LAUNCHED_DIRECT` and real D-Bus traffic (`DBUS_LISTNAMES` showing
    `xfce4-session`'s own registered bus names) — genuine forward progress, further than a bare
    crash — before free RAM fell from a healthy multi-GB baseline to ~105MB within the first 5
    seconds of monitoring. Terminated immediately via WMI (host never became unstable; RAM fully
    recovered to >7GB free after cleanup). This is the SAME pre-existing RAM-crater blocker Track B
    item 1 has chased since the 76th pass — genuinely unrelated to this pass's own fix (a single,
    fast data point, not a controlled A/B against the 88th pass's own single-generation code under
    identical host load) — not re-attempted this pass given the host-risk observed. `DE_UP` NOT
    reached.
  - **Known, explicit, deliberately-deferred follow-up (not attempted this pass)**: page protection
    is applied ONE PAGE AT A TIME in this landing (not batched per contiguous committed sub-range
    the way the 88th pass's single-generation version was) — trades some of the original mechanism's
    own measured syscall-count win for correctness-first simplicity in a brand-new concurrent path.
    Batch `VirtualProtect` across contiguous never-yet-open pages (the common case), falling back to
    per-page joins only under genuine multi-generation overlap on the same page.
- **99th pass — ran the controlled 3-run A/B the 98th pass explicitly flagged as missing, and found
  its single uncontrolled data point was NOT just unlucky host load: `787b139`'s multi-generation
  guard-cow rewrite is a real, 100%-reproducible regression on both axes the task asked about (RAM
  trajectory AND a brand-new correctness bug).** No code changed this pass — see item 1 above for
  the full numbers (3/3 identical 20-21.3s craters at 7 processes, vs. 88th/89th's 195-300s/9-16-proc
  stable baseline; 3/3 identical NEW `XCENSUS_PRE_DE rc=134` heap corruption in a plain
  fork-then-execve `python3` call, absent in the immediately-prior `pass96`/`pass97` logs under the
  same env). Deliberately did not attempt a blind fix: `lazy_fork_commit.rs`'s own addressing
  (`total_pages`/`group_slot_base`/`slot_index`), heal-then-reopen sequencing, and lock ordering
  (`GUARD_PAGE_REGISTRY` before `VIRTUAL_PROTECT_LOCK`, matched on both the fork-time
  `guard_one_page` and write-fault-time `guard_cow_write_fault_veh` paths) all read as internally
  consistent on inspection alone — the defect only manifests after MANY real, sequential (not
  concurrently-overlapping) prior fork claims have already cycled through the same parent's
  `GUARD_PAGE_REGISTRY`, a shape none of the 98th pass's own 3 isolated repros exercises, so it
  needs a live `cdb`/`LITEBOX_DIAG_LAZY_FORK_COMMIT=1` session against that specific shape rather
  than a guess. Also ran one exploratory `LITEBOX_LAZY_FORK_COMMIT=1`-alone (guard-cow off)
  isolation boot: inconclusive (a DIFFERENT anomaly — Xvfb/`XSOCK_WAIT_DONE` timeout, then `SIGSEGV`
  not `SIGABRT` — and no crater at all in its own ~30s self-terminating run), one data point, not
  re-run this pass. Explicitly did NOT retune `live_cross_process_fork_children`'s admission cap —
  the regression's own signature (heap corruption, not merely faster exhaustion) means a
  capacity/concurrency lever cannot fix it and could hide it; this is a `787b139` correctness bug,
  not a Track-B capacity question. `DE_UP` not attempted (both flags remain default OFF and this
  pass found new reasons not to flip them on for a real boot yet). Host RAM confirmed fully
  recovered (>7.4GB free, zero stray `litebox_runner...exe`) after every run via WMI `Terminate`.
Fully DONE (kept only as a marker so a future pass doesn't re-attempt): the minimal isolated
cross-process AF_UNIX repro; the `Network` shared-arena redesign's `socket_set`/
`LocalPortAllocator`/`closing_in_background`/`queued_for_closure` slice; DISPLAY/`getenv()` as the
`DE_FAILED` cause; AF_UNIX `connect()` `EAGAIN`-vs-`EINPROGRESS`; `pty_registry`/
`daemon_pty_masters` (`syscalls::pty::SharedPtyTable`, live-verified cross-process); fork's
fd-eligibility scan dropping a redirected 0/1/2 (`raw_fd_is_plain_stdio_device`);
`SharedUnixConnectQueue`'s cancel-on-first-non-blocking-miss gap (`UnixStreamState::Connecting`);
both Xvfb SIGSEGVs.

**Open, in rough priority order:**

1. **`xfwm4` now launches AND SURVIVES (75th, then genuinely confirmed 97th), the RAM crater is
   closed only when lazy-fork-commit is ON (76th-88th) UNDER THE PRE-98TH single-generation
   guard-cow code, the silent-whole-host-death bug class is FIXED (96th), and Bug 4 (the guard-cow
   TOCTOU that was the 91st-97th passes' real, misattributed blocker) has a general-case fix landed
   (98th pass) -- but the 99th pass's controlled follow-up found that general fix (`787b139`) is
   ITSELF a new, real, 100%-reproducible correctness regression, not a net win yet.** `DE_UP` still
   not reached.
   - **99th pass — the controlled A/B the 98th pass flagged as missing.** Rebuilt the release binary
     (confirmed mtime postdates `787b139`), ran the identical `de_only_xcensus_seed3.tar` harness/env
     the 96th-97th passes used (`LITEBOX_PROCESS_FORK=1 LITEBOX_LAZY_FORK_COMMIT=1
     LITEBOX_LAZY_FORK_GUARD_COW=1`, same `--env GLIBC_TUNABLES=...`, same 1s-granularity RAM/process
     poll) 3 times sequentially, host RAM 7.1-7.2GB free before each run, never more than one boot
     live at once. **Result, 3/3 IDENTICAL**: free RAM falls from ~7.1GB to the <1.0GB kill-switch
     threshold in **20-21.3s** (t=20.0s/0.60GB, t=21.2s/0.61GB, t=21.3s/0.59GB across the 3 runs),
     at exactly **7 `litebox_runner...exe` processes** each time — a severe regression from the
     88th/89th passes' own measured 2.8-4.6GB free / 9-16 processes sustained for a FULL 195-300s
     window under the pre-98th single-generation code, same harness. All 3 runs reached
     `DE_LAUNCHED_DIRECT` (past `DBUS_UP`) before the kill-switch fired; no `STATUS_ACCESS_VIOLATION`
     exit code was observed for any process (all "exit" codes were litebox's own `0xC0DE`-prefixed
     synthetic wrapper for clean exits/`SIGABRT`, decoded: `0xC0DE0000`=exit 0, `0xC0DE0086`=exit 134
     i.e. `SIGABRT`) — meaning `xfce4-session`/`gdbus` were never observed to independently crash;
     they were killed by our own RAM-crater WMI-terminate before any such crash could occur either
     way, so this run cannot say whether Bug 4's ORIGINAL target (the `xfce4-session` crash) is fixed
     under real boot conditions, only that the crater now arrives far too fast to find out.
   - **A NEW, 100%-reproducible correctness regression, found (not guessed) by comparing markers
     against the immediately-prior `pass96_boot{1,2,3}.out.log`/`pass97_releaseboot1.out.log`**: all
     3 of THIS pass's runs show `[s] XCENSUS_PRE_DE rc=134 >>>corrupted size vs. prev_size<<<` -- a
     real glibc heap-corruption `SIGABRT` inside `python3 /tmp/xcensus.py`, invoked via plain
     `$(python3 /tmp/xcensus.py)` command substitution, i.e. an ordinary fork-then-execve — exactly
     the case the 98th pass's own isolated repros claimed 5/5 clean on both builds. The 96th/97th
     passes' logs (same env-var configuration, pre-`787b139` code) all show `XCENSUS_PRE_DE rc=0`,
     clean, at the identical script line. Nothing else in the harness, env, or host changed between
     those runs and this pass's own -- the only variable is `787b139`'s multi-generation guard-cow
     rewrite. This is a genuine gap in the 98th pass's own verification: none of its 3 isolated
     repros (fork-then-execve `bash -c`, fork-without-execve subshell, 3-concurrent-subshell) happen
     to exercise a long-lived parent that has ALREADY serviced many earlier, unrelated, sequential
     (not concurrently-overlapping) guard-cow claims before the corrupting fork happens -- a real
     `/de_only.sh` boot forks `mkdir`/`rm`/`xset`/etc. many times before reaching `python3`, each a
     separate claim/release cycle through the SAME `GUARD_PAGE_REGISTRY` this file's own code reads
     in detail (`litebox_platform_windows_userland/src/lazy_fork_commit.rs`'s `guard_one_page`,
     `guard_cow_write_fault_veh`) without finding an obvious logic error on inspection alone --
     `total_pages`/`group_slot_base`/`slot_index` addressing, the heal-then-reopen sequence, and the
     lock ordering (`GUARD_PAGE_REGISTRY` then `VIRTUAL_PROTECT_LOCK`, matched on both the fork-time
     and write-fault-time paths) all look self-consistent by code reading; the actual defect is
     REAL (3/3, precisely marker-comparable against a clean baseline) but not yet isolated to a
     specific line without a live `cdb`/`LITEBOX_DIAG_LAZY_FORK_COMMIT=1` session against this exact
     sequential-many-prior-forks shape, which the 83rd-85th/88th passes' own precedent says this
     bug class typically needs.
   - **One exploratory isolation run** (`LITEBOX_LAZY_FORK_COMMIT=1` alone, `GUARD_COW` unset, same
     harness) is INCONCLUSIVE, not exonerating: it hit a DIFFERENT-looking anomaly first (`XSOCK_WAIT
     _DONE i=40 exists=no` -- Xvfb never became ready inside the 40s wait -- then `PROBE_XSET rc=139`/
     `XCENSUS_PRE_DE rc=139`, both `SIGSEGV` not `SIGABRT`) and never cratered at all (stable
     6.3-7.2GB free / 0-4 processes for its whole ~30s self-terminating run, reaching `DE_FAILED
     after 60s` on its own). One run is not enough to conclude plain lazy-commit-without-guard-cow is
     either safe or unsafe here — the Xvfb-startup anomaly may be unrelated host-load noise. Not
     re-run this pass (budget went to nailing down the guard-cow-on comparison, which had a clear
     controlled 3/3 baseline to compare against; this isolation angle does not).
   - **Next pickup, precise**: (a) a live `cdb`/`LITEBOX_DIAG_LAZY_FORK_COMMIT=1` session on the
     `python3 /tmp/xcensus.py` fork specifically, inside a real `/de_only.sh` boot (not an isolated
     repro), to find exactly which guarded page's snapshot/protection state is wrong by the time this
     fork happens — the isolated repros' own clean 5/5 results mean the bug needs REAL accumulated
     prior-fork state to reproduce, so a fresh minimal repro should explicitly chain several
     sequential, non-overlapping fork-then-execve children from one long-lived parent before the
     corrupting one, mirroring `/de_only.sh`'s own real shape, rather than testing forks in isolation
     again; (b) do NOT retune `live_cross_process_fork_children`'s admission cap in response to this
     — the regression's signature (heap corruption, not merely faster resource exhaustion) points at
     a correctness bug in `787b139`'s own per-page registry/snapshot logic, which a capacity/
     concurrency-cap change cannot fix and could mask; (c) re-run the 3-run controlled comparison
     with `LITEBOX_LAZY_FORK_COMMIT=1` alone (3 clean runs, not 1) once a repro exists, to properly
     settle whether Bug 4's TOCTOU (documented pre-98th) or `787b139`'s own new registry logic is the
     actual source, since this pass's single lazy-only run was inconclusive; (d) once fixed and
     re-verified 3/3 clean on the real boot (not just isolated repros — this pass's own finding is
     that isolated-repro-clean is no longer sufficient evidence for this mechanism), redo this exact
     controlled 3-run RAM-trajectory comparison to see whether the ORIGINAL question (did the crater
     get faster/slower/same from the multi-generation generalization, independent of the new
     corruption bug) can finally be answered. **`cdb -p` invasive attach with `sxd av` measurably
     induces its own severe exception-dispatch livelock** on this exact codebase (250,000+ repeated
     AVs in ~20s, absent undebugged, 97th pass) — prefer the existing `exception()`/
     `LITEBOX_DIAG_LAZY_FORK_COMMIT=1` `debug!`/`eprintln!` diagnostics over a live attach where
     possible, per that same precedent.
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
