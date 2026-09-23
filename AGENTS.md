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
mechanism; ~53.6KB→~44KB). 90th pass added a real fix (`VM_FOREIGN_LIVE_NEVER_REPLACE`, see its own
entry below) without further compaction — this file is over 30KB again; the next pass should drain
the 90th pass's own bullet (and older CLOSED material) to the archive once its own follow-up work
lands, rather than compacting a still-actively-being-extended entry.

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
  - **89th pass — RAM-fix reproducibility CONFIRMED across 2 more independent real boots (debug
    build, both flags on); DE_FAILED's proximate cause found to be REAL, live `xfce4-session`
    process crashes in 2 of 3 runs (not a passive "WM registration gap") -- but root-caused these
    crashes as PRE-EXISTING, thread-based-fork-only corruption, UNRELATED to
    `lazy_fork_commit`/guard-cow; a first-ever live `cdb` attach on a cross-process fork child was
    achieved but did not catch the fault in the act; `DE_UP` NOT reached.** Rebuilt both debug and
    release (`cargo build [--release] -p litebox_runner_linux_on_windows_userland`, both exactly
    current for `96234c2`). Two fresh debug-build `de_only_xcensus_seed3.tar` boots (both flags on,
    `LITEBOX_DIAG_FATALDUMP=1`, both watchdogs disabled) each ran their FULL monitoring window
    (~283s and ~300s+) with a stable 2.8-4.6GB free / 4-8 processes band, zero crater -- a second and
    third independent confirmation of the 88th pass's result, different binary (debug vs. release),
    reproducible.
    - **Real finding, from re-reading the 88th pass's own `pass88_boot1` logs plus two fresh debug
      boots**: `xfce4-session`'s own real Windows process (its cross-process-fork guest pid IS its
      real winpid, confirmed `winpid=` in `task-resume-probe`) CRASHED with a genuine unhandled
      exception in 2 of 3 runs -- `[wait4_diag]` shows `exit_code=3221225477` (`0xC0000005`
      `STATUS_ACCESS_VIOLATION`) for BOTH `xfce4-session` (pid 18388) and `ssh-agent` (pid 22932,
      its child) within the 88th pass's own release-build log (`.wfgy/pass88_boot1.err.log:2092,
      2094`), 16-31s after each process's own thread started; a fresh debug-build run showed a
      DIFFERENT exit code, `exit_code=3221225485` (`0xC000000D` `STATUS_INVALID_PARAMETER`), for
      `xfce4-session` itself 31.3s after a `clone()` to `/bin/sh` to `iceauth` chain (scratch log,
      not committed). Confirmed via a count of `exit_code=3221225477` that this is RARE (3 of 82
      `wait4_diag` exits in the 88th pass's own log), not the normal encoded-exit sentinel
      (`0xC0DE0000`/`3235774464`, seen 160 times) -- a real distinguishing signal, not noise. **This
      directly explains `DE_FAILED`/the empty `_NET_SUPPORTING_WM_CHECK`: the session manager that
      would launch `xfwm4` is dead before it ever gets there**, not merely "hung" or stuck on a
      passive registration gap as the 88th pass's own writeup guessed (reasonably, since it did not
      check `wait4_diag` exit codes).
    - **Root-caused (via `DIAG_TIMELINE clone`'s own `child_tid=` vs `winpid=` marker, plus
      `classify_lazy_eligible_groups`/`try_claim_guard_cow_table`'s own call sites in
      `process_fork.rs:1825,1849`) that EVERY crashing fork in both runs is on the OLD, PRE-EXISTING
      THREAD-BASED fork fallback path (`child_tid=`), never cross-process (`winpid=`)** -- confirmed
      `xfce4-session`'s own `clone()`s for `ssh-agent`/`/bin/sh` are thread-based (the same
      ineligible-fd reason `ssh-agent`'s own daemonizing fork already logs:
      `kinds=["unix-socket"]`, unrelated to `xfce4-session`'s OWN fork but the same mechanism).
      `lazy_fork_commit`/guard-cow ONLY ever runs for cross-process fork children
      (`FORK_CHILD_LAZY_RANGES_ENV_VAR`/`FORK_CHILD_GUARD_COW_TABLE_ENV_VAR` are only set on the
      cross-process spawn path, `process_fork.rs`) -- these specific crashes cannot be caused by the
      83rd-88th passes' own new mechanism. The far more likely suspect is the SAME long-documented,
      still-not-fully-solved thread-based-fork corruption class this file has flagged for months
      (ADVISORY-001 §3N tcache-safe-linking corruption and/or `fork_verify.rs`'s own stale-pointer
      healing) -- both logs show `fork_verify: stale CODE pointer detected` / `AV-path stale rip
      livelock detected, falling through to deeper slot healers` dozens of times throughout, i.e.
      this OLD healing machinery is actively, heavily engaged around the crash window, not merely
      present. **Not yet proven which exact one -- a real, still-open gap.**
    - **First-ever live `cdb` attach on a cross-process fork child, achieved but inconclusive**:
      `cdb` (`C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe`, not on `PATH`) attaches
      fine to both the root runner and a cross-process-forked child once the PID is genuinely still
      alive -- the FIRST `-pv` (noninvasive) attempt failed with `No runnable debuggees` on `g` not
      because of any permission problem but because `-pv` never takes ownership of the debug loop
      (`g` has nothing to continue) -- use a plain invasive `-p <pid>` attach (still always detach
      with `qd`, never bare `q`, per this file's own standing rule) if the goal is to catch a live
      in-progress fault with `sxe`/`g`. An automated log-tailing watcher script that greps for
      `argv0=/usr/bin/xfce4-session`'s `winpid=` and attaches within ~1s was necessary -- manual
      multi-tool-call polling is too slow to win the race against a crash that lands 16-31s after
      thread start. Even with this, the one successful invasive attach never caught the fault: the
      target ran its entire observed window (300s+, well past its own un-debugged 16-31s crash
      point) without crashing OR producing its own usual early stderr (the `[de2]`-tagged
      `libxfce4util-WARNING`/EWMH-query lines that appear within ~1s in an un-debugged run were
      completely ABSENT the whole run) -- i.e. a sustained invasive `cdb` attach measurably changes
      this specific heavily-multi-threaded target's own behavior/timing (consistent with, but not
      proof of, the underlying bug being genuinely timing-sensitive; could also simply be `cdb`'s own
      suspend/resume overhead delaying this target's very early startup). **Concrete pickup for a
      future pass with more live-debug budget**: retry the same auto-attach approach but breaking in
      EARLY (right after the `task-resume-probe` line, before the target has done much) and manually
      single-stepping forward a bounded number of instructions rather than a blanket `g`, to avoid
      the same timing perturbation; also worth directly testing whether
      `--env GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0` (this file's own
      documented thread-based-fork tcache-corruption workaround) is genuinely present in
      `xfce4-session`'s/`ssh-agent`'s/`iceauth`'s own real process environment at the point of their
      OWN `clone()` (several generations removed from the container's root entrypoint through
      `/de_only.sh` -> `xfce4-session` -> `clone()`) -- not directly checked this pass, a real gap,
      and the most concrete remaining hypothesis: if a real `xfce4-session`/`GLib`
      environment-rebuild step silently drops it before spawning `ssh-agent`, the exact crash class
      this workaround exists to prevent would recur even with the flag correctly passed at the CLI.
    - Both `LITEBOX_LAZY_FORK_COMMIT`/`LITEBOX_LAZY_FORK_GUARD_COW` stay default OFF, unchanged this
      pass -- nothing above is a regression in either flag's own mechanism; this pass's finding is
      that a SEPARATE, older bug class is what stands between the RAM fix and `DE_UP`, not the new
      lazy/guard-cow mechanism itself, which continues to check out clean on every re-run.
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
- **90th — pursued Angle 1 (why does `xfce4-session`'s crashing fork go thread-based, not
  cross-process); found the exact answer with real evidence, fixed two genuine, previously-unknown
  architectural bugs the answer led to (both live-verified), but the ORIGINAL crash survives both
  fixes unchanged — narrower, still-open finding, not a full close.** Added permanent diagnostics
  first rather than guessing: `pid`/`comm` on the `try_cross_process_fork` ineligibility warn (was
  `tid`-only), and a new warn at `do_clone`'s `vforked` branch point stating which of the two
  same-process fallbacks (`vforked=true` shared-PM vs. `vforked=false` eager-duplicate) a given
  fork actually took. **Real evidence, not a guess**: `xfce4-session`'s own crashing fork (its
  `/bin/sh` startup-script child) is a genuine `CLONE_VFORK` (`vforked=true`) — `do_clone`'s own
  `if !vforked && self.try_cross_process_fork(...)` gate means `try_cross_process_fork` is never
  even CALLED for it; this is categorically different from the 89th pass's "thread-based, not
  cross-process" framing, which did not distinguish vfork-shared from eager-duplicate within that
  bucket.
  - **Bug A (FOUND+FIXED, real, live-verified): `detach_pm_for_vfork_execve`
    (`litebox_shim_linux/src/syscalls/process.rs`) built the vfork child's fresh `PageManager` via
    plain `PageManager::new` — blind to every range the STILL-LIVE, merely-blocked parent currently
    occupies.** Windows has no per-guest-process page tables (unlike real Linux's `vfork`+`execve`,
    where the child's `exec_mmap` installs a genuinely separate `mm_struct` with zero collision
    risk against the parent's own, untouched one) — a same-process vfork child's own ELF/stack
    placement can select a REAL address the parent is still using, and `insert_mapping`'s existing
    `FixedAddressBehavior::Replace` path (`litebox/src/mm/linux.rs`) silently decommits-then-
    recommits over it, corrupting the parent. Fixed: new `Vmem::new_for_vfork_execve_detach` /
    `PageManager::new_for_vfork_execve_detach` seed the child's fresh `Vmem` with the OLD (still-
    shared) `PageManager`'s own `tracked_regions()`, tagged with a new `VmFlags::
    VM_FOREIGN_LIVE_NEVER_REPLACE` bit that `insert_mapping`'s `Replace` arm now rejects
    UNCONDITIONALLY on any overlap (not just the existing partial-overlap-with-real-content case,
    which an ordinary empty-flags `Vmem::new` placeholder is deliberately exempt from — see that
    flag's own doc comment for why the two must stay distinct). **This mechanism is REAL and fires
    on a genuine, previously-silent collision**: `xrdb`'s own `cpp`→`cc1` vfork chain (both classic
    non-PIE GCC binaries loading at the SAME fixed `0x400000`) — `cc1`'s `execve` now correctly
    fails loud (`LoadError(Map(Errno(ENOMEM)))`, `AddressPartiallyInUse`) instead of silently
    overwriting `cpp`'s own still-live, about-to-resume image, which a prior pass's own comment
    (the `vfork-child-execve-large-elf-enomem` investigation) had misdiagnosed as merely straddling
    a harmless reserved placeholder — it is not, in this case it is real live parent content, a
    genuinely new, previously-undetected instance of this whole investigation's core bug class.
  - **Bug B (a real regression Bug A itself introduced — FOUND live, FIXED, live-verified): marking
    those seeded ranges non-empty made `release_memory`'s existing `!vm.is_empty()`-based "this is
    real, mine, free it" predicates (`process.rs`'s `prepare_for_exit` and `sys_execve`, both
    already special-cased for `VM_OWN_FORK_PADDING`) sweep them up too.** A vfork grandchild's own
    ordinary exit/exec teardown then issued a genuine platform `deallocate_pages`/`VirtualFree`
    against the PARENT's real memory — caught live or by direct log evidence: one such attempt hit
    an entry covering Windows' own `KUSER_SHARED_DATA` page (`0x7ffe0000`), producing a real Rust
    panic (`WindowsUserland::deallocate_pages`: "The handle is invalid") whose unwind took down the
    ENTIRE real Windows process — including `Xvfb`'s own unrelated main X-server-serving thread
    (same-process vfork sharing means one thread's fail-fast kills every thread), observed as
    `Xvfb` exiting `STATUS_STACK_BUFFER_OVERRUN` (`0xC0000409`, this codebase's own documented
    `RaiseFailFastException` signature) ~4s after its own start, cascading into every later X
    client seeing "unable to open display". **Fixed**: both `release` closures now also exclude
    `VM_FOREIGN_LIVE_NEVER_REPLACE`; `Vmem::duplicate`'s own region-copy filter (a plain ordinary
    `fork()` from an already-vfork-detached process) excludes it too, for the same reason (it is
    not this process's own content to copy). **A second, related over-broad-protection bug found
    and fixed in the same pass**: the FIRST version of Bug A's fix carried forward an ancestor's
    own EMPTY-flags `Vmem::new`/`reserved_pages()` placeholders (genuinely foreign-but-legitimately-
    stealable, e.g. the very `0x400000` band `cc1` needs) as `VM_FOREIGN_LIVE_NEVER_REPLACE`
    (wrongly upgrading "stealable" to "never touch"), which broke `cc1` outright
    (`LoadError(ENOMEM)` on ITS OWN legitimate, first, non-colliding load). Fixed by filtering
    `parent_occupied` to non-empty-flags entries only (`detach_pm_for_vfork_execve`'s own updated
    doc comment has the full before/after). **Live-verified, both fixes together, 2 full boots
    (`de_only_xcensus_seed3.tar`, ~220s each)**: `Xvfb` no longer crashes (X server stays reachable
    — `_NET_SUPPORTING_WM_CHECK` queries now return "no such atom on any window", not "unable to
    open display"); `xfce4-session` reaches real GTK/ICE startup (`ConsoleKit proxy` warning,
    `_NET_NUMBER_OF_DESKTOPS`/`_NET_WORKAREA`/`_NET_CURRENT_DESKTOP` EWMH queries, `iceauth`
    authority file creation) — materially further than any run this pass observed before either fix
    landed; `cc1`'s own real, first-use collision correctly rejects loud rather than corrupting
    silently, with `xrdb`'s own failure being a narrow, non-blocking, cosmetic side effect (X
    resource preprocessing) that does not stop `DE_LAUNCHED_DIRECT`.
  - **The ORIGINAL target crash — `xfce4-session` itself, `STATUS_ACCESS_VIOLATION`
    (`exit_code=3221225477`), ~16-16.7s after its own start, immediately after its own `vfork()` of
    `/bin/sh` — is UNCHANGED by any of the above, confirmed by re-running the exact same boot with
    both fixes landed (2/2 runs where `xfce4-session` reaches this point at all: identical signature,
    16685ms/16691ms elapsed).** Decisive negative evidence, not a guess: the new
    `VM_FOREIGN_LIVE_NEVER_REPLACE` rejection log line NEVER fires anywhere in `xfce4-session`'s own
    fork chain across every run this pass captured (it fired exactly once, total, across all boots —
    for the unrelated `cc1` case above) — ruling OUT a `Replace`-mode fixed-address placement
    collision as this specific crash's mechanism. The vfork chain itself (`xfce4-session`→`/bin/sh`→
    `iceauth`, all exiting/execve-ing cleanly, `status=0`) completes with no visible error; the crash
    is DELAYED well past it, with zero corresponding `[veh]`/`diag-unrecov-av`/panic output anywhere
    in the log for `xfce4-session`'s own winpid — meaning this specific fault is NOT being caught by
    this codebase's own VEH machinery at all (contrast `Xvfb`'s crash above, which WAS: a real
    Rust panic with a full backtrace). Root cause remains OPEN.
  - **Next pickup, precise**: (1) a live `cdb -pv`/`-p` attach specifically on `xfce4-session`'s own
    winpid, breaking EARLY (right after its `task-resume-probe` line) and single-stepping a bounded
    window rather than a blanket `g`/`sxe` — the 89th pass's own sustained invasive attach ran the
    whole window without ever reproducing the crash (timing-perturbation-sensitive), so bound the
    step count and consider breaking specifically around the `vfork`-resume point (`wait_for_vfork_
    done` returning) rather than at process start. (2) Since no VEH output at all appears for this
    fault, check whether it is happening on a HOST-mode code path the VEH's own `is_in_guest`-keyed
    triage doesn't classify as one of its "four codes" at all (`docs/veh-exception-handler-design.md`)
    — i.e. confirm whether this is even a GUEST-mode fault before assuming it's a guest memory bug.
    (3) The 89th pass's own still-unchecked `GLIBC_TUNABLES` propagation question remains open and
    cheap to check first.

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
   **89th pass: RAM-fix reproducibility CONFIRMED (2 more independent clean real boots, debug
   build) -- but `DE_FAILED`'s own root cause is now KNOWN to be, in most runs, a real
   `xfce4-session` process CRASH (real `STATUS_ACCESS_VIOLATION`/`STATUS_INVALID_PARAMETER` exit
   codes via `wait4_diag`), not a passive registration gap -- and that crash is on the OLD
   thread-based-fork path (`child_tid=`), NOT `lazy_fork_commit`/guard-cow (`winpid=`-only). See
   this section's own "89th pass" entry above for the full evidence chain.**
   **Superseded by the 90th pass update below** (kept one line for the trail: this list's own item
   (2), the early-breakpoint bounded-single-step `cdb` approach, is still the live recommendation).
   `LITEBOX_DIAG_FORK_VMA_BREAKDOWN=1` (zero cost when off) remains the permanent tool for measuring
   any future fix's real payoff. `DE_UP` has not been reached by any pass through the 90th;
   chrome-devtools MCP was not re-checked this pass (no boot got close enough to a real desktop to
   make it worth checking). Lower-priority, still
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
   **90th pass update**: root-caused the crashing fork to a genuine `CLONE_VFORK`, fixed two real
   architectural gaps this uncovered (a vfork child's blind fresh address space risking silent
   parent-memory corruption on `execve`, and a regression that fix itself caused in `release_memory`/
   `Vmem::duplicate`) — both live-verified (`Xvfb` no longer crashes; `xfce4-session` now reaches real
   GTK/ICE startup). **The original `xfce4-session` crash itself is UNCHANGED by either fix** —
   decisive evidence (the new collision-rejection log line never fires for its own fork chain) rules
   out a placement collision as this specific crash's cause; see this file's own "90th" pass-history
   entry above for the full evidence chain and precise next pickup (an early-breakpoint `cdb` attach
   on `xfce4-session`'s own winpid, and checking whether this fault is even guest-mode at all).
   `DE_UP` still not reached.
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
