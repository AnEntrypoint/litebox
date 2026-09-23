# litebox — current state (2026-09-23)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, or one a later commit superseded, is deleted rather than hedged. Reference
detail is drained to `docs/AGENTS_ARCHIVE_*.md` and dated `docs/*.md` in the map below — read those
for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one
line plus its pointer, not a separate memory file. Compacted at the 65th, 70th, 72nd, 75th, 76th,
81st, 83rd, 85th, 88th, 91st and 93rd passes (pass-history section below; 26th-69th full narrative:
`docs/AGENTS_ARCHIVE_2026-09-22.md`; 70th-92nd full narrative, including each pass's own complete
evidence and fix rationale: `docs/AGENTS_ARCHIVE_2026-09-23.md`). Re-compacted 93rd pass (drained
the 91st pass's own full writeup to the archive now that the 92nd pass superseded its conclusion,
same pattern as the 91st pass's own prior compaction of 88th-90th). Still over the 30KB target —
this file's own "Docs and tooling map"/"Closed" sections and older CLOSED pass-history entries are
the next drain candidates for a future pass, once no actively-being-extended entry would be
disturbed (the 93rd pass's own item-1 entry is exactly such an actively-extended entry — do not
drain it until its own `FS_BASE` pickup is resolved one way or the other).

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
- **91st — root-caused the 90th pass's "ZERO `[veh]`/panic output" mystery to a real, independently
  doubly-documented `GS_BASE` corruption class (Windows' exception dispatcher itself needs valid
  `GS_BASE` to even invoke a registered VEH callback) and fixed the one call site that lacked its
  repair (`RawMutex::block_or_maybe_timeout`'s `WaitForSingleObject` loop, what `wait_for_vfork_done`
  blocks in) — but could NOT live-verify against the real crash this pass (3/3 boots died from an
  earlier, unrelated `bash pid=80` tcache crash first). Full mechanism, doc-comment cross-references,
  exact fix: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "91st pass full narrative" section.
- **92nd — the `bash pid=80` crash did not reproduce (0/5 boots), so the 91st pass's `GS_BASE` fix
  could finally be tested against the real `xfce4-session` crash — and was DISPROVED live: a new
  `LITEBOX_DIAG_GS_BASE_REPAIR=1` diagnostic fired zero times across 3 full boots while the
  identical `STATUS_ACCESS_VIOLATION` crash still occurred every time. Found the exact, fully
  deterministic faulting guest RIP via Windows' own Application Error event log
  (`rip=0x00007fefe92bc7cb code=0xC0000005`, byte-identical across all 3 independent boots) — real,
  reproducible, invisible to litebox's own VEH logging. Ruled out lazy-fork-commit/guard-cow as a
  simple on/off cause (live test: removing guard-cow makes things categorically worse, not better).
  Full evidence, exact commands and the precise next-pickup cdb target: this file's own item-1 entry
  under "Open, in rough priority order", below.
- **93rd — live-captured the exact deterministic crash (`rip=0x00007fefe92bc7cb`) with `cdb` for the
  first time ever, via a hardware execute breakpoint at the 92nd pass's own known address (invasive
  `cdb -p <winpid>`, NOT `-pv` — confirmed live that `-pv` cannot receive debug events at all,
  "the process can be examined but debug events will not be received"; `-pv` is for read-only
  inspection of an already-alive process, invasive `-p` + `qd` to detach is what actually catches a
  breakpoint). Identified the code: guest glibc's `__syscall_error` (`neg eax; mov rcx,[tls-offset-
  slot]; mov fs:[rcx],eax; or rax,-1; ret` — the Initial-Exec-model `errno = -ret` store every failed
  syscall wrapper calls), `rcx=0xffffffffffffffa0` (a small negative TLS offset). `!address @rip`:
  `Usage: <unknown>`, `PAGE_EXECUTE_READ`, `MEM_PRIVATE` — real guest-mapped memory, matching the
  92nd pass's event-log `Faulting module name: unknown`. cdb's own effective-address preview for the
  faulting `fs:[rcx]` resolved to the literal offset with NO base contribution
  (`fs:ffffffff`ffffffa0=????????`) — i.e. `FS_BASE` reads back `0` on this thread at this exact
  instruction, the guest-TLS-register sibling of the already-documented `GS_BASE`-clears-under-
  scheduling-pressure class. **Fix attempted (real, safe, landed, but empirically NOT sufficient)**:
  `RawMutex::block_or_maybe_timeout`'s own comment already flagged `FS_BASE` as equally at risk at
  that exact call site, yet only `GS_BASE` had ever gotten a repair there (91st pass) — added the
  mirroring `WindowsUserland::restore_thread_fs_base()` call (`litebox_platform_windows_userland/
  src/lib.rs`, right after the existing `restore_thread_gs_base_if_cleared()` call). Builds clean
  (debug + release). **Live-verified this does NOT fix the crash**: a fresh, fixed release-binary
  boot (no debugger) still crashed `xfce4-session` at `elapsed_ms_since_thread_start=16993`,
  `exit_code=3221225477`, byte-identical to every pre-fix run — the FS_BASE clearing this specific
  crash depends on is not occurring (at least not exclusively) via that call site. A genuinely useful
  negative result: `RawMutex::block`'s wait loop is closed as a cause for THIS crash (the fix itself
  stays landed — it is still a real, independently-justified gap-closer per the code's own prior
  comment, just not the explanation here). **Deeper, not-yet-conclusive finding via single-stepping
  through the live fault** (`t` repeatedly past the breakpoint, `sxd av` NOT set so cdb stops on every
  first-chance AV): on this same thread, in the seconds/instructions immediately before the fatal
  write, FIVE OTHER `fs:`-relative guest READS (`fs:[0x18]`, `fs:[0x18]` again from a different call
  site, `fs:[r12]`/`fs:[r14]` both `=-0x40`, `fs:[0x28]` — the classic glibc stack-protector canary
  check) ALL show the identical "no FS_BASE contribution" signature and yet do NOT immediately kill
  the process the way the final WRITE (`__syscall_error`'s `mov fs:[rcx],eax`) does — suggesting
  `FS_BASE` may be clearing far more often on this thread than previously characterized (not one
  isolated event near a long vfork wait, but seemingly every few guest instructions), with reads
  perhaps surviving via the existing reactive `vectored_exception_handler` repair-and-retry
  (`lib.rs:2509-2539`, gated on `WindowsUserland::get_thread_fs_base() != 0` — skips repair entirely,
  falling to the fatal path, if litebox's OWN recorded value for this thread is itself `0`) while a
  WRITE either does not get the same retry treatment or loses a race the reads happen not to. **This
  observation needs treating with real caution, not as confirmed fact**: it was made WHILE invasively
  attached, and the 89th pass already established that invasive attachment measurably perturbs this
  exact bug class's own timing — the cascade-of-reads-then-one-fatal-write pattern could be an
  artifact of debugger-added latency rather than the true undebugged sequence. **Next pickup,
  precise**: (a) re-run the identical single-step trace 2-3 more times to see if the "5 reads then 1
  write" shape is stable, or an artifact of this one capture; (b) read `lib.rs:2509-2539` (the
  guest-mode FS_BASE repair) and `lib.rs:1802-1919` (the host-mode sibling) side by side against a
  REAL write-fault case to determine definitively whether the repair-and-retry path treats a write
  destination any differently from a read source (it should not, by inspection — `wrfsbase`+retry
  just re-executes the same instruction regardless of read/write — so if the mechanism really is
  identical, the "write is special" theory from this pass is likely wrong and the true answer is
  timing/race-window-based instead, worth checking `WindowsUserland::get_thread_fs_base()`'s value
  captured at the exact moment of the fatal fault specifically, not inferred from the debugged
  trace); (c) a `LITEBOX_DIAG_FS_BASE_REPAIR=1`-style permanent diagnostic (mirroring the 92nd pass's
  own `LITEBOX_DIAG_GS_BASE_REPAIR`) on the GUEST-mode repair site specifically, run WITHOUT a
  debugger attached at all, would settle both (a) and (b) with real, unperturbed evidence — this is
  the single most valuable next step, cheaper and less invasive than more cdb sessions. `DE_UP` not
  reached. Two release+one debug boot this pass, all cleanly self-terminated or WMI-`Terminate`d, RAM
  never fell below ~1.4GB free, no concurrent boots.
- **94th — implemented the 93rd pass's own recommended `LITEBOX_DIAG_FS_BASE_REPAIR=1` diagnostic
  (mirroring `LITEBOX_DIAG_GS_BASE_REPAIR` exactly), got real unperturbed evidence, and used it to
  FIND+FIX two genuine, previously-unpatched GS_BASE/FS_BASE repair gaps in `RawMutex` — both real,
  safe, independently justified, and both LIVE-VERIFIED INSUFFICIENT for this specific crash, which
  is the pass's actual headline finding: the crash's ~16.4-16.9s timing is suspiciously IDENTICAL
  across 4 different repair-coverage configurations, arguing against the "random scheduling-pressure
  MSR clear" framing every fix through the 93rd pass has assumed.** `DE_UP` NOT reached.
  - **Diagnostic added** (`litebox_platform_windows_userland/src/lib.rs`): `VehGates::fs_base_repair`
    (`LITEBOX_DIAG_FS_BASE_REPAIR`), logging at FOUR sites: the guest-mode AV repair (both the
    successful-repair case AND, newly, the "detected the exact reset shape but `THREAD_FS_BASE`
    itself reads 0, cannot repair" case — the previously-unlogged half of the story), its
    single-step-path sibling, and the host-mode AV repair's own mirrored pair, plus an unconditional
    "host-mode AV, no FS_BASE-reset match" catch-all. A shim-side companion
    (`litebox_shim_linux/src/syscalls/process.rs`'s `ThreadInitState::ForkedChild` handler) logs
    every forked/vforked child's own FS_BASE establishment for cross-correlation.
  - **First real finding (decisive negative result, confirmed twice independently)**: across two
    full `de_only_xcensus_seed3.tar` boots with this diagnostic on, NONE of the new log lines ever
    fired anywhere near `xfce4-session`'s own crash — not the guest-mode repair, not the host-mode
    repair, not the "can't repair" failure case, not even the codebase's own PRE-EXISTING, totally
    unconditional `[diag-unrecov-av]`/`[diag-veh-no-tls]` prints (`lib.rs`, no gate at all) that fire
    for ANY unrecovered guest-mode fault reaching this codebase's own VEH. This independently
    reconfirms, via a completely different mechanism, the 90th pass's own original "zero `[veh]`/
    `diag-unrecov-av`/panic output anywhere in the log for `xfce4-session`'s own winpid" finding
    (`docs/AGENTS_ARCHIVE_2026-09-23.md:1289`) and the 91st pass's independently cross-session-
    confirmed theory (`docs/AGENTS_ARCHIVE_2026-09-03.md:5643`): this codebase's own
    `vectored_exception_handler` is never even being INVOKED for the fault that kills
    `xfce4-session` — consistent with Windows' own exception dispatcher itself needing a valid
    `GS_BASE` to locate the TEB/VEH chain and reach ANY registered callback at all, so sufficiently
    bad `GS_BASE` corruption is invisible to every diagnostic living inside the VEH (cdb's own live
    capture, 93rd pass, bypasses this because a kernel debug port does not need the same
    `GS_BASE`-relative TEB lookup ntdll's own userspace SEH/VEH dispatch does).
  - **Two real fixes landed on that theory** (`RawMutex`, `litebox_platform_windows_userland/src/
    lib.rs`): (1) `finish_real_timeout`'s own `WaitForSingleObject(event, INFINITE)` — reached only
    via the narrow "a real timeout raced a concurrent `wake_many`" branch — had NO repair call at
    all, unlike its sibling in `block_or_maybe_timeout`'s main loop; added both
    `restore_thread_gs_base_if_cleared()` and `restore_thread_fs_base()` immediately after it. (2)
    `poll_until_value_changes` (the `MAX_INLINE_WAITERS`-exhaustion fallback, entered whenever
    `RawMutex::block_or_maybe_timeout` logs "waiter queue full, falling back to polling") is a pure
    `std::thread::sleep`-based spin loop with NO repair call anywhere — added both calls per
    iteration. Live-caught this path actually firing on a real boot: `max_waiters=32` reached at
    15.24s into one run, ~1.66s before that same run's `xfce4-session` crash at 16.9s — real,
    demonstrably-live evidence this fallback is active under real XFCE-startup lock contention, not
    a theoretical path.
  - **Both fixes live-verified INSUFFICIENT, twice**: post-fix-1-only boot still crashed at
    elapsed_ms=16900/16901 (byte-identical exit code, `owning_pid=27256`=`xfce4-session`); post-both-
    fixes boot still crashed at elapsed_ms=16754 (`owning_pid=3384`=`xfce4-session`) — and in that
    second run "waiter queue full" never even fired, so fix 2 wasn't exercised that specific run
    either way. Across all 4 measurements this pass and the 93rd pass combined (pre-fix 16468/16993,
    post-93rd-fix-alone 16864, post-94th-fix-1 16900, post-both-94th-fixes 16754), the spread is
    under 550ms regardless of which `RawMutex` repair coverage is present — real, reproducible,
    unperturbed evidence against "this crash is a random Windows scheduling-pressure MSR clear
    racing a `RawMutex` wait", the framing every fix attempt from the 91st through this pass has
    shared. A genuinely random race raced against 4 different code changes affecting its own
    contention/timing characteristics would be expected to show more than 550ms of jitter.
  - **Sharpened pickup for the next pass**: the timing's own suspicious consistency now outweighs
    the segment-base-MSR-clear theory as the leading explanation for THIS specific crash (the two
    fixes landed this pass remain real, general, worth keeping regardless). Two concrete next
    angles, neither yet attempted: (a) audit every OTHER brand-new-OS-thread-creation path (beyond
    `ThreadInitState::ForkedChild`, already confirmed correct this pass, and `NewThread`'s
    `tls: Some(_)` case, also confirmed correct) for one that can legitimately leave `THREAD_FS_BASE`
    at its Rust-default `0` — a genuinely uninitialized value reads identically to a "cleared" one
    to every existing repair site's `saved != 0` guard, and would produce this exact
    byte-identical-every-time signature far more naturally than a race would; `NewThread`'s
    `tls: None` case (clone() without `CLONE_SETTLS`) is unaudited and worth checking even though
    real glibc/musl `pthread_create` should always pass `CLONE_SETTLS` in practice. (b) Search for a
    Windows-side or litebox-side constant near 15-17s that could explain the timing being fixed
    rather than random — `EXTERNAL_GRACE_PERIOD` (`process_fork.rs`, 15s) and
    `SHARED_UNIX_CROSS_CONNECT_TIMEOUT` (`unix.rs`, 15s) are structurally close but unconfirmed;
    neither has been checked against whether `xfce4-session`'s own fork tree ever actually exercises
    them. A live `cdb -p` attach breaking on `exception_callback`'s entry (not `vectored_exception_
    handler`, which the 84th pass separately found is instrumentation-sensitive) remains the
    only way to see PAST this pass's own decisive "VEH is never entered" finding, if the next pass
    has budget for the perturbation risk the 93rd pass already flagged. Three release boots this
    pass (baseline capture, post-fix-1, post-both-fixes), all cleanly self-terminated or
    WMI-`Terminate`d, RAM never observed below ~3.9GB free, no concurrent boots.
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
   writable-layer export-path fallback bug (75th); the RAM crater (76th-88th, lazy fork-memory
   population + single-generation guard-page COW, both default OFF behind `LITEBOX_LAZY_FORK_
   COMMIT=1`/`LITEBOX_LAZY_FORK_GUARD_COW=1` — a real boot ran its full ~195s window without
   cratering, reaching `DE_LAUNCHED_DIRECT` → `WM_POLL` → a real X window). Full narrative for
   75th-90th: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "88th-90th pass full narrative" section and
   this file's own compacted 88th-90th pass-history entry above.
   **Current blocker (90th-91st pass)**: `xfce4-session` itself dies `STATUS_ACCESS_VIOLATION`
   ~16-16.7s after start, immediately after its own genuine `CLONE_VFORK` of `/bin/sh`, with ZERO
   `[veh]`/panic output — 90th pass fixed two real, unrelated vfork-detach architectural bugs
   (`VM_FOREIGN_LIVE_NEVER_REPLACE`) that this crash survived unchanged. **91st pass root-caused
   the "zero VEH output" mechanism**: independently DOUBLY-documented (`docs/
   AGENTS_ARCHIVE_2026-09-03.md:5643`, cross-referenced this pass) `GS_BASE` corruption under
   "nested vfork() kernel-transition pressure" — the OS's own exception dispatcher needs valid
   `GS_BASE` to even locate/invoke a registered VEH callback, so when it breaks badly enough
   (confirmed independently via `ntdll!mov %gs:0x60,%rcx` faulting), Windows' dispatcher faults
   before litebox's VEH ever runs. Landed the fix that prior finding's own writeup specified: GS_BASE
   repair now also runs after every `WaitForSingleObject` wakeup inside `RawMutex::
   block_or_maybe_timeout` (`litebox_platform_windows_userland/src/lib.rs`), not just at VEH/
   syscall-entry — closes the gap for `wait_for_vfork_done`'s own many-seconds-long blocking wait
   (previously unrepaired). **NOT yet live-verified against the target crash**: 3/3 real boot
   attempts this pass (1 pre-fix baseline, 2 post-fix) all died from a DIFFERENT, earlier,
   already-known `bash` crash (`pid=80`, general ADVISORY-001 §3N thread-fork tcache corruption,
   identical in the pre-fix baseline too — not a regression) before `xfce4-session` was ever
   reached.

   **92nd pass — the `bash pid=80` early crash did NOT reproduce (0/5 boots), letting the GS_BASE
   fix finally be tested against the real target, and DISPROVED it with live evidence.** Added a
   cheap, permanent, zero-cost-when-off diagnostic (`LITEBOX_DIAG_GS_BASE_REPAIR=1`,
   `VehGates::gs_base_repair`, resolved once via the existing pre-resolved-gate pattern, not a
   fresh `std::env::var_os` inside the VEH itself) that logs every time `restore_thread_gs_base_
   if_cleared` actually observes-and-repairs a cleared `GS_BASE`, at all three call sites. **Result,
   3 independent full boots with it on: ZERO repairs fired in any run, yet `xfce4-session` crashed
   with the IDENTICAL `STATUS_ACCESS_VIOLATION` signature every time** (`elapsed_ms_since_thread_
   start` 17160/16542/16402) — `GS_BASE` is never observed cleared at `vectored_exception_handler`'s
   entry, `syscall_handler`'s entry, or `block_or_maybe_timeout`'s wait loop during this crash's
   whole lifetime, so the 91st pass's fix, while real and harmless, does not explain or resolve
   this specific crash. **New decisive evidence via Windows' own Application Error event log**
   (`Get-WinEvent -FilterHashtable @{LogName='Application';ProviderName='Application Error'}`,
   filtered to `Faulting module name: unknown` = a guest-address fault) pins down the exact faulting
   guest RIP across THREE independent boots: **`rip=0x00007fefe92bc7cb code=0xC0000005`, byte-for-
   byte IDENTICAL every time**, cross-checked against each run's own `task-resume-probe (child,
   winpid=…)` line to confirm each one really is that run's own `xfce4-session` (winpids
   0x4710/0x63B0/0x5778). A deterministic, identical crash RIP across independent boots (different
   fork trees, different timing) rules out random heap corruption as the direct mechanism and points
   at a specific, reproducible code path — yet litebox's own runner log has ZERO `[veh]`/
   `[diag-unrecov-av]` output for this fault in all three runs, even with `LITEBOX_DIAG_FATALDUMP=1`
   on throughout (confirmed still firing correctly for every OTHER real fault in the same logs) —
   the fault reaches the OS's real unhandled-exception path (WER) while staying totally invisible to
   litebox's own instrumentation. **Ruled out lazy-fork-commit/guard-cow as a simple on/off cause,
   tested live**: `LITEBOX_LAZY_FORK_COMMIT=1` alone (guard-cow unset) does not fix this — it causes
   markedly WORSE, widespread early corruption instead (114+ processes exiting via signal almost
   immediately, zero `DIAG_TIMELINE execve` entries logged at all) and `xfce4-session` itself dies as
   an immediate, loud, correctly-delivered GUEST `Segmentation fault` (bash's own job-control
   message) rather than the silent host AV — matching the already-documented Bug 4 TOCTOU risk for
   fork-without-`execve` on real long-lived daemons, and confirming guard-cow is necessary (removing
   it makes things broadly worse), not itself simply "introducing" this narrower residual crash from
   nothing. Both flags fully OFF cannot be tested within the RAM budget (craters to <1GB free by
   `WM_POLL n=4`, well before `xfce4-session` could reach its own 16-17s crash window — confirmed
   live, aborted via WMI `Terminate` on a falling-RAM trend per this file's own safety rule). Net
   effect: falsifies the 91st pass's leading theory with real evidence, and produces the deterministic
   RIP the 93rd pass then used as a live `cdb` breakpoint target.

   **93rd pass — first-ever live `cdb` capture of this exact crash address, root cause narrowed to
   the guest's `FS_BASE` (Linux TLS base register) reading `0` at the fault, a fix attempted and
   landed but empirically NOT sufficient, and a real, specific next step identified.** Key correction
   to the 92nd pass's own "next pickup": **`cdb -pv` CANNOT catch a breakpoint at all** — confirmed
   live, `-pv`'s own output says so verbatim ("the process can be examined but debug events will not
   be received") — invasive `cdb -p <winpid>` (detach cleanly with `qd`, never bare `q`) is required
   to actually stop on a breakpoint; `-pv` is read-only inspection of an already-running process. With
   invasive attach + `ba e1 0x00007fefe92bc7cb` + `g`, the breakpoint hit on the FIRST attempt against
   a fresh, rebuilt release binary — no `exception_callback`-entry breakpoint or symbol resolution
   needed, the raw address was enough. Disassembly: guest glibc's `__syscall_error`
   (`mov rcx,[addr]; neg eax; mov fs:[rcx],eax; or rax,-1; ret` — the standard errno-store after any
   failed syscall), faulting on the STORE, with `rcx=0xffffffffffffffa0`. `!address @rip`:
   `Usage: <unknown>`/`PAGE_EXECUTE_READ`/`MEM_PRIVATE` (real guest memory, matching the 92nd pass's
   `Faulting module name: unknown`). cdb's own `fs:` effective-address preview resolved to the bare
   offset with no base contribution — `FS_BASE` reads `0` on this thread at the fault, the guest-side
   sibling of the already-fixed `GS_BASE`-clears-under-scheduling-pressure class. **Fix landed** (real,
   safe, builds clean both profiles): `RawMutex::block_or_maybe_timeout`'s own pre-existing comment
   already named `FS_BASE` as equally at risk at that call site, but only `GS_BASE` had a repair there
   — added `WindowsUserland::restore_thread_fs_base()` alongside it. **Live-verified NOT sufficient**:
   a fresh post-fix release boot still crashed at the identical `elapsed_ms_since_thread_start=16993`,
   `exit_code=3221225477` — closes `RawMutex::block` as a cause for THIS crash specifically (fix stays
   landed as a real, independently-justified gap-closer for whatever it does cover). Single-stepping
   live through the fault (invasive `cdb`, `t` repeatedly, first-chance AV breaking enabled) showed
   FIVE OTHER `fs:`-relative guest READS in the same thread's immediately-preceding instructions (a
   stack-protector `fs:[0x28]` canary check among them) with the identical zero-base signature that
   did NOT immediately kill the process, vs. the ONE write that did — but this was observed WHILE
   invasively attached, which the 89th pass already showed perturbs this exact bug's timing, so treat
   this "reads survive, the write doesn't" pattern as a lead, not a conclusion (94th pass: STILL
   unconfirmed either way, superseded as the leading theory — see below).

   **94th pass — implemented the 93rd pass's own recommended `LITEBOX_DIAG_FS_BASE_REPAIR=1`
   diagnostic, got real unperturbed evidence, found+fixed two genuine `RawMutex` GS_BASE/FS_BASE
   repair gaps, and both were LIVE-VERIFIED INSUFFICIENT — decisive new evidence that the
   "segment-base MSR randomly cleared under scheduling pressure" framing (91st-93rd passes'
   shared premise) is very likely the WRONG mechanism for this specific crash.** Full mechanism,
   exact fixes, exact numbers: this file's own 94th pass-history entry above. Summary: (1) the
   new diagnostic independently RECONFIRMED the 90th pass's "zero VEH output" finding via a
   completely different method — not just the targeted GS_BASE/FS_BASE repair sites but this
   codebase's own PRE-EXISTING, totally unconditional `[diag-unrecov-av]`/`[diag-veh-no-tls]`
   prints (no gate at all) also never fire, meaning `vectored_exception_handler` is never even
   invoked for this fault, undebugged. (2) Found and fixed two real, previously-unpatched gaps —
   `RawMutex::finish_real_timeout`'s own `WaitForSingleObject(event, INFINITE)` (reached via a
   narrow real-timeout-races-`wake_many` branch) and `RawMutex::poll_until_value_changes` (the
   `MAX_INLINE_WAITERS`-exhaustion "waiter queue full" fallback, a pure `std::thread::sleep` spin
   loop) — both had zero GS_BASE/FS_BASE repair anywhere, unlike their sibling call sites; the
   second was LIVE-CAUGHT actually firing 1.66s before a real crash (`max_waiters=32`), proving
   it's a real, active path, not theoretical. (3) **Both fixes together still did not move the
   crash at all**: pre-94th baseline 16468/16993ms, 92nd pass's own 3 boots 17160/16542/16402ms,
   post-fix-1-only 16900ms, post-both-fixes 16754ms — **7 independent boots across 4+ distinct
   code versions, spread under 800ms** (`owning_pid` confirmed as `xfce4-session` every time).
   This tightness is itself the pass's real finding: a genuine scheduling-pressure RACE would be
   expected to show more jitter across code changes that alter contention/timing characteristics;
   this looks more like a fixed timeout or a deterministic (not probabilistic) uninitialized-value
   bug. **Next pickup, precise, two untried angles**: (a) audit every remaining brand-new-OS-
   thread-creation path for one that can leave `THREAD_FS_BASE` at its Rust-default `0` rather than
   a genuine hardware clear of a previously-good value — `ThreadInitState::ForkedChild` and
   `NewThread`'s `tls: Some(_)` case are both confirmed correct (94th pass, by code reading);
   `NewThread`'s `tls: None` case (`process.rs:7034`, clone() without `CLONE_SETTLS`) is the one
   remaining unaudited case, low-probability (real glibc/musl `pthread_create` always passes
   `CLONE_SETTLS`) but unchecked. (b) `EXTERNAL_GRACE_PERIOD` (`process_fork.rs`, 15s) and
   `SHARED_UNIX_CROSS_CONNECT_TIMEOUT` (`unix.rs`, 15s) are the only two 15-17s-range constants
   found by a first grep pass — neither confirmed nor ruled out against whether `xfce4-session`'s
   own fork tree exercises them; a wider search (including GLib/D-Bus/xfce4-session's OWN default
   timeouts, guest-side, not litebox's) has not been done. A live `cdb -p` attach breaking on
   `exception_callback`'s own entry (never `vectored_exception_handler` itself — the 84th pass
   found that function is instrumentation-sensitive enough that new probes there need an A/B test
   against a non-crashing repro first) remains the only way to see past this pass's "VEH never
   entered" finding, budget/perturbation-risk permitting.
   `GLIBC_TUNABLES` propagation to `xfce4-session`'s own environment (89th/90th) remains untested.
   `DE_UP` has not been reached by any pass through the 94th; chrome-devtools MCP was not
   re-checked this pass (no boot got close enough). Lower-priority, still open: (a) decompose
   remaining per-fork cost between rootfs materialization staying resident post its cheap
   (~83-140ms) build vs. Windows loader overhead; (b) use `de_only_xcensus_seed3.tar`'s working
   `/tmp/xcensus.py` (`XCENSUS_SELECTION`/`XCENSUS_ROOTPROP`) + `LITEBOX_DIAG_SOCKET_READ_TARGET=
   xfwm4` to check whether the ~10.7s `GetAllProperties` retrigger (69th/70th, still unconfirmed)
   recurs; (c) unconfirmed: `LITEBOX_LOG` may not reach forked children's own stderr — see
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
