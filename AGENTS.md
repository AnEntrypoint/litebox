# litebox -- current state (2026-09-24)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, or one a later commit superseded, is deleted rather than hedged. Reference
detail is drained to `docs/AGENTS_ARCHIVE_*.md` and dated `docs/*.md` in the map below -- read those
for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one
line plus its pointer, not a separate memory file. Compacted at the 65th, 70th, 72nd, 75th, 76th,
81st, 83rd, 85th, 88th, 91st, 93rd, 95th, 98th, 101st and 104th passes (pass-history section below;
26th-69th full narrative: `docs/AGENTS_ARCHIVE_2026-09-22.md`; 70th-100th full narrative, including
each pass's own complete evidence and fix rationale: `docs/AGENTS_ARCHIVE_2026-09-23.md` -- the
91st-102nd condensed bullets below cite this same archive for their own full narrative, further
trimmed by the 104th pass since nothing below drops information not already there). Still over the
30KB target; "Docs and tooling map"/"Closed" remain further drain candidates for a future pass.

**Where things stand, in one paragraph (updated 105th pass)**: cross-process fork
(`LITEBOX_PROCESS_FORK=1`) alone remains solid and the default-safe path -- `xfwm4` launches and
survives (no crash) with real X11 windows created, but the boot still hits the long-standing
pre-83rd-pass RAM crater (Track B item 1) before `DE_UP`. **`LITEBOX_LAZY_FORK_COMMIT=1
LITEBOX_LAZY_FORK_GUARD_COW=1` is STILL NOT safe and must NOT be used for a real boot attempt.**
The 104th pass fixed the sigreturn-trampoline page being wrongly merged into a LAZY-ELIGIBLE data
group (`classify_lazy_eligible_groups`/`lazy_commit_veh`). **The 105th pass found and fixed a
SECOND, independent instance of the exact same underlying class of bug, in a completely different
module**: `fork_verify.rs`'s own AV-path/single-step stale-CODE-pointer healer -- the safety net
pass 142/143 wired into EVERY real cross-process fork child via `run_thread_with_fork_verification`
(using an IDENTITY `AddressRelocations`, so `is_in_source(rip)` is true for nearly every address the
child touches) -- had no awareness of the trampoline address at all, so it "healed" (translate-and-
resume, a no-op under an identity map) the SAME deliberate fault and infinite-refaulted, live-
confirmed via `.wfgy/pass105_xset_repro.sh` at `rip=translated_rip=0x7feffffef000`, "AV-path stale
rip livelock detected". **This corrects the 104th pass's own working hypothesis**: the 84th pass's
"interaction with `fork_verify.rs`'s own watched-code-page machinery" was about the diagnostic-only
`codewatch` module (always off by default) and does not apply; the REAL interaction is that pass
142/143 deliberately (and correctly, as a general safety net) wired `fork_verify`'s stale-pointer
healer into the cross-process path too, and it simply needed the same sigreturn-trampoline exclusion
`lazy_commit_veh` already had. Fixed by threading `Task::sigreturn_trampoline_addr()` through
`ForkChildVerificationProvider::begin_fork_child_verification` (new parameter) into `fork_verify.rs`,
which now declines to heal `rip == sigreturn_trampoline` at both call sites that can heal a code
pointer (`on_single_step` case (1), `translate_stale_source_rip`). **Live-verified, real A/B on a
rebuilt release binary**: 0 occurrences of the `0x7feffffef000`/livelock pattern after the fix (was
20/dozens before); `LITEBOX_PROCESS_FORK=1` alone unaffected (0 fatal signals, `xset rc=0`, confirming
the new plumbing is a genuine no-op on the non-identity thread-based path). **NOT closed: the SAME
repro, same fix applied, STILL shows 6 fatal SIGSEGVs and `xset rc=139` with both lazy flags on** --
a genuinely DIFFERENT, still-open bug (zero `0x7feffffef000` hits in this run, so it is not another
instance of the trampoline pattern). New evidence this pass: the clearest instance crashes
immediately after a `DIAG_TIMELINE execve` into `/usr/bin/rm`, preceded by a
`[diag-recover-fsbase] recover_rip=... fsbase=...` line -- an FS_BASE-recovery event right at the
guest's post-`execve` resume. Root cause not yet found. **Both lazy-fork flags remain default OFF.
`DE_UP` has not been reached by any of the 105 passes to date.** See "Cross-process fork"'s own
"Open, in rough priority order" item 1 for the precise next pickup.

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
- **91st-97th (compacted; full narrative: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "91st-94th"/"95th-97th
  pass full narrative" sections) — a 7-pass misattribution, resolved.** 91st-94th chased a "Windows
  clears `GS_BASE`/`FS_BASE` under scheduling pressure" theory: found+fixed several real `RawMutex`
  register-repair gaps (kept landed, real and general), but reached a decisive negative result —
  across 4 repair-coverage configurations the crash timing stayed within ~550ms (too tight for a
  real scheduling race) and no repair diagnostic, not even the VEH's own catch-all print, ever fired
  near the crash. 95th-96th traced the VEH assembly, refuted a `CONTEXT_SEGMENTS` idea by hard fact
  (AMD64 `CONTEXT` has no `FsBase`/`GsBase` field) and fixed a real gap (`switch_to_guest` never
  repaired `GS_BASE` before resume, unlike `FS_BASE`) — the silent whole-host death became a
  contained per-thread crash. **97th overturned the working theory entirely**: the real,
  reproducible crash is a plain `CLONE_THREAD` pthread (`comm=gdbus`) hit by the
  ALREADY-DOCUMENTED `lazy_fork_commit.rs` Bug 4 TOCTOU, confirmed via a clean A/B (0/1 crashes with
  the lazy flags removed vs. 2/2 with them) — the entire 91st-96th FS_BASE/GS_BASE chase, while each
  individual fix is real, was chasing a misattributed symptom of Bug 4. Also fixed, 97th: `VEH_FRAME_
  STRIDE`/`EXCEPTION_RECORD_RESERVE` were sized only for release codegen, crashing every debug-build
  boot in ~3s via the project's own overflow canary — widened 8x under `#[cfg(debug_assertions)]`,
  making debug-build live debugging viable for the first time.
- **98th-101st (compacted; full narrative: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "98th-100th pass
  full narrative" section, plus this file's own git history for the 101st).** 98th generalized Bug
  4's fix to any number of concurrent per-parent lazy-fork generations (`GUARD_PAGE_REGISTRY`,
  one shared open interval per page rather than per-generation shadow versions), fixing a livelock
  of its own (a page left `PAGE_READONLY`-poisoned when its last pending generation died unhealed)
  before landing 5/5 clean. **99th turned one promising uncontrolled real-boot data point into a
  controlled 3-run A/B and found a real, 100%-reproducible regression on two axes**: craters at
  20-21.3s/7 processes (vs. the pre-98th-pass 195-300s/9-16-proc baseline) AND a new `XCENSUS_PRE_DE
  rc=134` heap-corruption signature in a plain fork-then-execve `python3` call. **100th root-caused
  `rc=134` to Bug 7** (`sys_execve` never calls `disarm_on_execve`, so a process that was EVER a
  lazy-fork child keeps servicing faults against its ORIGINAL parent across arbitrarily many future
  unrelated `execve()`s) and fixed it plus two more real bugs from code reading (Bug 6a: O(pages)
  `OpenProcess` handle churn; Bug 6b: `GUARD_PAGE_REGISTRY` desyncing from ordinary guest `mprotect`/
  `munmap`). `rc=134` confirmed gone post-fix, but a DIFFERENT, older `rc=139` SIGSEGV resurfaced,
  and crater speed stayed unchanged. **101st implemented+verified the batched-`VirtualProtect`
  optimization** (`try_guard_region_batched`, real syscall-count win, kept landed) and confirmed by
  real boot A/B it does NOT fix the crater-speed regression (real negative evidence — the crater is
  driven by cumulative `VirtualAlloc2(MEM_COMMIT)` charge, not guard-cow's own overhead) — and
  reconfirmed `rc=139`/Angle B is real and broad (6-28 events across many distinct commands within
  ~15s) but explicitly deferred root-causing it to a live `cdb` session rather than guess further.
  Both flags stayed default OFF throughout 98th-101st; `DE_UP` not reached by any of them.
- **102nd — root-caused and FIXED the crater-speed regression by direct code reading (not guessing),
  verified with 2/2 real boots; the `rc=139` bug (Angle B, superseded by the 103rd/104th passes'
  finer diagnosis below) confirmed still real and unaffected.** Root cause: every successful
  guard-cow claim `Box::leak`s a full-page `GuardSnapshotSlot` table per guarded guest page
  (accepted tradeoff since the 88th pass), and the 98th pass's generalization from one outstanding
  claim per parent to unboundedly many (bounded only by the UNRELATED 6-wide
  `live_cross_process_fork_children` cap) removed an undocumented incidental rate limit the old
  single-owner gate used to provide — a busy parent could now accumulate up to 6x as much
  simultaneously-live leaked commit as before. Fix: `GUARD_COW_OPEN_CLAIMS`/
  `GUARD_COW_CONCURRENT_CLAIM_CAP: u32 = 3` (`lazy_fork_commit.rs`) restores rate-limiting,
  generalized to N>1 so it doesn't reintroduce the 98th pass's own TOCTOU fix. Verified: isolated
  8-concurrent-subshell repro shows the cap firing exactly as designed (5/8 declined, all 8 still
  correct, no cross-contamination); real boot A/B (`de_only_xcensus_seed3.tar`, release, 2/2 runs)
  went from the unfixed code's 14-21s/7-process crater-to-kill-switch to RAM recovering and
  stabilizing at 5 processes/~3.6-4.2GB free for a full ~188s run, exiting cleanly rather than being
  RAM-killed — matching the pre-98th-pass baseline's own order of magnitude. Both flags stayed
  default OFF (fix only active when explicitly set). Cap value (3) not yet empirically tuned.
  `rc=139`/Signal(11) reconfirmed real (44 events/boot, both runs) and unaffected by this fix — not
  root-caused this pass (time-boxed to the crater-speed fix); see 103rd/104th for the finer-grained
  diagnosis that supersedes this pass's "does not block boot progress" read (it does, for
  `xfce4-session` itself). Logs: `.wfgy/pass102_realboot_run{1,2}.{out,err,poll}.log`,
  `.wfgy/pass102_concurrent{,_run2}.log`, `.wfgy/pass102_lazy_execve.log`.
- **103rd -- the first pass to directly investigate `DE_FAILED` itself (per the standing gap all
  102 prior passes left open). Found `rc=139`/`STATUS_ACCESS_VIOLATION` under the lazy-fork config
  IS the direct cause of `DE_FAILED` there (correcting the 102nd pass's "does not block boot
  progress" read); separately, re-verified the plain non-lazy path against the CURRENT binary and
  found it gets genuinely further (`xfwm4` launches and survives) but still hits the pre-83rd-pass
  RAM crater before `DE_UP`; found+fixed a stale-seed-tar repo-hygiene bug. No source code changed
  this pass -- `LITEBOX_PROCESS_FORK=1` alone stays byte-identical by construction.**
  - **`DE_FAILED`'s own definition, confirmed by direct reading (not assumed)**: `.wfgy/de_only.sh`'s
    own poll loop (`WM=$(xprop -root _NET_SUPPORTING_WM_CHECK ...)`, 12x5s=60s) declares
    `DE_FAILED after 60s` unless `$WM` ever contains the literal substring `"window id"` -- i.e.
    exactly the `_NET_SUPPORTING_WM_CHECK` atom existing on the root window with a real value, set
    by `xfwm4` alone (confirmed by the 82nd pass's own source reading of `setNetSupportedHint`).
    This pass's own new evidence (below) directly answers, for the first time, WHY that atom never
    gets set within the window, under both configurations.
  - **Lazy-fork config (`LITEBOX_PROCESS_FORK=1 LITEBOX_LAZY_FORK_COMMIT=1
    LITEBOX_LAZY_FORK_GUARD_COW=1`) -- `DE_FAILED` root-caused: `xfce4-session` itself crashes before
    ever launching `xfwm4`.** Re-examined the 102nd pass's own "stable" real-boot logs
    (`.wfgy/pass102_realboot_run{1,2}.err.log`, reused unmodified) with a finer-grained read than
    that pass attempted: `[wait4_diag] wait_for_thread_exit(...) owning_pid=23636 ...
    exit_code=3221225477(real_exit_code)` -- `3221225477 == 0xC0000005 ==
    STATUS_ACCESS_VIOLATION` -- fired at `elapsed_ms_since_thread_start=16169`, i.e.
    `xfce4-session`'s OWN cross-process-fork child process died of an UNHANDLED HOST-LEVEL fault
    (not a guest-translated `SIGSEGV` -- no `[unix_addr_presence]`/`fatal signal: terminating task`
    line exists for this pid at all, unlike every OTHER crashing process in the same log, which DOES
    get gracefully translated to a reported guest signal). The piped `[de2]`-prefixed
    `xfce4-session` stderr (`pass102_realboot_run2.out.log:18-26`) shows it reaches exactly:
    `ConsoleKit proxy` warning, three `_NET_*`-property "assuming" messages, `iceauth: error in
    locking authority file`, `Failed to setup the ICE authentication data` -- and NOTHING further.
    `argv0=/usr/bin/xfwm4` (or any other Failsafe client) never appears ANYWHERE in either run's
    `DIAG_TIMELINE execve` log. **`xfce4-session` dies of a real, silent, unhandled host crash
    between its own ICE-auth warning and its first session-client launch, in BOTH of the 102nd
    pass's own "stable" runs, 2/2.**
  - **Isolated, much cheaper reproduction of the SAME crash class, unrelated to `xfce4-session`
    entirely**: `.wfgy/pass103_xset_repro.sh` (+ `.wfgy/pass103_xset_seed.tar`) -- just `Xvfb` +
    `xset q`, no `dbus`/`xfce4-session` at all. With both lazy flags ON, this is NOT a rare/narrow
    bug: `xset`, `xprop`, `rm`, `mkdir`, and `sleep` ALL crash across repeated runs (real guest
    `SIGSEGV`, litebox's own `syscalls::signal` correctly reports "fatal signal: terminating task"
    for these, unlike `xfce4-session`'s silent host-level death above) -- essentially any
    forked-then-exec'd child is at risk, not only the fork-without-execve subshell case the 83rd/84th
    passes originally scoped this bug to. 17 of 25 captured fault events (`LITEBOX_DIAG_FATALDUMP=1
    LITEBOX_DIAG_FAULT_VQ=1 LITEBOX_DIAG_FAULT_MODULE=1`, `.wfgy/pass103_xset_repro1.log`) share the
    IDENTICAL fault address `0x7feffffef000` -- a CODE-FETCH fault (`rip==cr2`), on a page
    `VirtualQuery` reports as already `MEM_COMMIT`+`PAGE_READONLY` (not the `MEM_RESERVE` state an
    unfaulted lazy range should show), with `lazy_commit_veh`'s own group lookup logging "meta-slot
    parent-side: no reverse translation found" -- the address isn't tracked as a lazy-reserved range
    at all, yet real Windows memory state shows something already touched and mis-protected it.
    **Decisive control**: the IDENTICAL script with ONLY `LITEBOX_PROCESS_FORK=1` set (lazy flags
    unset) shows ZERO crashes across the same commands, `xset q` returns real output rc=0
    (`.wfgy/pass103_xset_repro_nolazy.log`) -- proving the crash is caused BY the lazy flags, not
    pre-existing. This IS the SAME mechanism as the 100th-102nd passes' "Angle B `rc=139`"
    (previously found via `winpid=8916` crashing "essentially immediately after task-resume-probe",
    `.wfgy/pass102_lazy_execve.log`) -- this pass shows it is NOT narrow/non-blocking: it killed the
    actual session manager in the real boot. **Correction to the 102nd pass's own assessment**
    ("`rc=139` does NOT itself block a boot from proceeding"): that was true only in the sense that
    OTHER processes crashing didn't visibly stop `WM_POLL`/`XCENSUS` from cycling -- but the SAME bug
    hitting `xfce4-session` itself is fatal to reaching `DE_UP`. Root cause of the fault itself
    (why this address ends up committed+`PAGE_READONLY` and untracked) is NOT found this pass --
    needs the live `cdb` attach the 101st/102nd passes already recommended, now using this pass's
    much cheaper `xset`-under-`Xvfb` repro instead of a full real boot or even a subshell.
  - **Non-lazy path (`LITEBOX_PROCESS_FORK=1` alone) re-verified against the CURRENT binary --
    genuinely further progress than any lazy-flag run, but still RAM-craters before `DE_UP`.** No
    pass since the 76th/77th/82nd passes' RAM fixes had re-tested this exact combination on a real
    boot. `.wfgy/pass103_nolazy_boot1.{out,err,poll}.log`: `xfce4-session` does NOT crash (no
    `exit_group`/`exit_signal` for its pid anywhere in the log), and `argv0=/usr/bin/xfwm4` DOES
    `execve` (pid 22036) and stays alive with no exit event for the rest of the run -- real,
    confirmed progress no lazy-flag run reaches. `XCENSUS_WINDOWS total=9` by `WM_POLL n=2`
    (real X11 windows exist). The boot still hits the pre-83rd-pass RAM crater: 16 processes /
    0.17GB free at t=123.9s, killed by the harness's own kill switch, before
    `_NET_SUPPORTING_WM_CHECK` is ever observed set. **So under the safe, non-lazy path, `DE_FAILED`
    is NOT a WM crash or hang -- it's the same still-open RAM-crater problem (Track B item 1) cutting
    the boot off mid-initialization**, now reconfirmed on the current binary rather than assumed
    unchanged since the 82nd pass.
  - **Repo-hygiene bug found+fixed**: `.wfgy/de_only_xcensus_seed3.tar`'s embedded `de_only.sh`
    predates the 82nd pass's RAM-saving session trim (Failsafe cut to `xfwm4`+`xfsettingsd`,
    `at-spi-dbus-bus`/`pulseaudio` autostart hidden) -- per this file's own standing lesson
    ("`.wfgy/webtop_seed.tar` embeds a FROZEN COPY... re-tar after every edit"), it was never
    re-tarred after that pass landed the trim in the host-side `.wfgy/de_only.sh`. **Every
    `de_only_xcensus_seed3.tar` boot since the 82nd pass -- including BOTH of the 102nd pass's own
    "stable" verification runs -- has actually been running the FULL untrimmed 5-client Failsafe
    session** (confirmed via `DIAG_TIMELINE execve`: `iceauth`, `ssh-agent`, `at-spi-bus-launcher`,
    `dbus-update-activation-environment`, `gpgconf` all appear, none of which the trim should allow).
    Fixed: `.wfgy/pass103_de_only_trimmed_seed.tar` merges the current (trimmed) `de_only.sh` with
    the working xcensus-via-stdin probe and a lighter `WM_POLL` loop (10s interval, census only at
    the end -- the loop's own `sleep`+`xprop`+`python3` forks were competing with `xfce4-session`'s
    tree for the same RAM/admission-slot budget during exactly the window under test). Re-tested with
    the trim genuinely applied + lazy flags off: the crater still occurs (~90-111s, 10-13 processes)
    -- consistent with, not contradicting, the 82nd pass's own "tested, confirmed real but
    exhausted... neither reduces Windows' own `VirtualAlloc2(MEM_COMMIT)` charge" conclusion; the
    trim reduces window/client count but doesn't move the crater's RAM math. **Open note for next
    pass, not yet root-caused**: even with the corrected seed, one trimmed run's `DIAG_TIMELINE`
    still showed `xfce4-panel`/`Thunar`/`gpgconf` PATH-search `execve` attempts -- the trimmed
    `xfce4-session.xml` override may not be taking effect as designed; don't assume the trim is
    100% effective without checking this directly first.
  - **New lead for the RAM-crater problem itself, not yet investigated**: in both non-lazy runs,
    `xfwm4` spent its entire observed lifetime repeatedly hitting `[unix_addr_presence] ECONNREFUSED
    but address IS bound, by a DIFFERENT guest pid` (the ALREADY-DOCUMENTED "Open" item 2
    cross-process AF_UNIX data-plane sharing gap) roughly once every 1-10s, with no other visible
    progress before the crater killed it. Plausible, unconfirmed: this retry loop is why `xfwm4`
    is slow enough to lose the race against the RAM crater -- if so, fixing the item-2 AF_UNIX gap
    could matter more for reaching `DE_UP` than continuing to chase the lazy-fork-commit bug above.
    Check directly next pass: does `xfwm4` ever get PAST this retry loop given enough time (e.g. a
    boot with `live_cross_process_fork_children`'s cap temporarily lowered to slow the crater,
    purely as a diagnostic, not a fix), or is it stuck retrying forever?
  - Logs: `.wfgy/pass103_xset_repro.sh`, `.wfgy/pass103_xset_seed.tar`,
    `.wfgy/pass103_xset_repro1.log` (lazy ON, fault diagnostics), `.wfgy/pass103_xset_repro_nolazy.log`
    (lazy OFF control), `.wfgy/pass103_nolazy_boot1.{out,err,poll}.log`,
    `.wfgy/pass103_de_only_trimmed_xcensus.sh`, `.wfgy/pass103_de_only_trimmed_seed.tar`,
    `.wfgy/pass103_trimmed_boot{1,2}.{out,err,poll}.log`.
- **104th -- confirmed the 103rd pass's `0x7feffffef000` hypothesis with a targeted diagnostic (not a
  guess), root-caused and FIXED one real, independent bug behind it, live-verified on the cheap
  `xset` repro; discovered the fix does NOT close the repro (a separate, larger, pre-existing bug --
  `fork_verify`'s own stale-CODE-pointer healer -- still crashes it pervasively) and sharpened that
  open item with new exact evidence instead of forcing an unverified fix.**
  - **Root cause, confirmed by direct evidence**: the address `0x7feffffef000` is the child's own
    `Task::ensure_sigreturn_trampoline` page (`litebox_shim_linux/src/syscalls/signal/mod.rs`) -- a
    REAL guest `mmap()` VMA (so it is a normal entry in `vma_layout`/`group_relocations`, subject to
    `Vmem::duplicate`'s 64 MiB `max_intra_group_gap` merge into whichever nearby data group is
    within that huge a gap), mapped `PROT_READ` only on x86_64 and DELIBERATELY never executable --
    the whole signal-return recognition mechanism (`LinuxShimEntrypoints::exception`: `ctx.rip ==
    sigreturn_trampoline_addr()`) depends on a fetch there always hardware-faulting. Because the
    page carries no `VM_EXEC` flag, `classify_lazy_eligible_groups`'s `has_exec` check can never
    exclude its group, so it could be classified lazy along with whatever data VMA it got merged
    with. Once lazy, `lazy_commit_veh` INTERCEPTS the deliberate trap fault (a genuine `LAZY_RANGES`
    hit), "services" it (`VirtualAlloc(MEM_COMMIT, PAGE_READWRITE)`, real byte copy from the parent,
    `EXCEPTION_CONTINUE_EXECUTION`) instead of letting it reach `LinuxShimEntrypoints::exception()`
    unserviced -- the retried fetch faults again identically forever (Windows DEP: neither
    `PAGE_READWRITE` nor `PAGE_READONLY` is executable), a 100%-reproducible infinite refault loop,
    never reaching the real handler. A DIFFERENT mechanism from the 85th pass's own Bug A (that one
    was about `CONTEXT.Rsp` at an unrelated exception's delivery time; this is about the trampoline
    page itself being touched on purpose) -- confirmed, not assumed, by extending
    `LITEBOX_DIAG_LAZY_FORK_COMMIT=1`'s existing diagnostic and re-running the exact
    `.wfgy/pass103_xset_repro.sh` repro on a fresh debug build: `[lazy_fork_commit] group
    0x7feffffb0000..0x7fefffff0000 excluded from lazy: contains
    sigreturn_trampoline=0x7feffffef000` -- the EXACT address from the 103rd pass's own crash dump.
  - **Fix (`litebox_platform_windows_userland/src/lazy_fork_commit.rs`,
    `litebox_platform_windows_userland/src/process_fork.rs`)**: `classify_lazy_eligible_groups` gains
    a `sigreturn_trampoline: usize` parameter (the SAME value `spawn_process_fork_child` already
    threads through for the 84th pass's unrelated Bug 2 fix, now reused for a second, independent
    purpose) and excludes whichever group contains it, mirroring the existing active-`%rsp`
    exclusion exactly. Belt-and-suspenders defense in depth also added to `lazy_commit_veh` itself:
    an `EXECUTE`-type access violation (`ExceptionInformation[0] == 8`) landing inside a tracked lazy
    range now declines (`EXCEPTION_CONTINUE_SEARCH`) unconditionally rather than servicing it --
    this module only ever marks PURE-DATA groups lazy, so a legitimate lazy fault should never be an
    instruction fetch; committing `PAGE_READWRITE` and retrying can never satisfy one, so failing
    safe is strictly better than a silent infinite loop for any FUTURE case in this same bug class.
    Both changes are entirely inside the `lazy_fork_commit_enabled()`-gated path (provably inert with
    both flags unset, same guarantee this module has always carried).
  - **Verified on the cheap repro, both with and without guard-cow**: rebuilt debug
    (`cargo build -p litebox_runner_linux_on_windows_userland --target x86_64-pc-windows-msvc`,
    confirmed fresh mtime), ran `.wfgy/pass103_xset_repro.sh` under
    `LITEBOX_PROCESS_FORK=1 LITEBOX_LAZY_FORK_COMMIT=1 LITEBOX_LAZY_FORK_GUARD_COW=1
    LITEBOX_DIAG_LAZY_FORK_COMMIT=1` (`.wfgy/pass104_xset_fix.{out,err}.log`) and again with
    guard-cow off (`.wfgy/pass104_xset_fix2.{out,err}.log`): the trampoline-group exclusion line
    fires correctly on essentially every fork (dozens of occurrences), ZERO
    `0x7feffffef000`-address faults anywhere in either log, and zero "declined execute-type fault"
    lines (meaning the classify-time fix alone is already sufficient -- the defense-in-depth check
    in `lazy_commit_veh` never even needed to fire). Also confirmed the crate compiles clean and
    `classify_lazy_eligible_groups`'s early return for `!lazy_fork_commit_enabled()` is unchanged, so
    `LITEBOX_PROCESS_FORK=1` alone stays byte-identical by construction (same guarantee this module
    has always carried; not re-verified via a fresh full boot this pass given the time cost, but the
    code path is unreachable when the flag is unset).
  - **NOT closed: the SAME guard-cow-off run still shows 48 `fatal signal: terminating task
    signal=Signal(11)` events, and the script's own `[s] PROBE_XSET rc=139` line shows `xset` itself
    still crashes** -- fixing the trampoline bug did not make the repro clean. This is the
    already-documented "Angle B `rc=139`" class (100th-103rd passes), now sharpened with new,
    precise evidence: every one of the 48 crashes is immediately preceded by 8 consecutive
    `fork_verify: stale CODE pointer detected via raw access violation (no #DB delivered),
    translating and resuming rip=... translated_rip=... fault_addr=18446744073709551615 repeat=1..8`
    lines (the SAME constant `rip=0x110097b30`/`fault_addr=-1` pair across every distinct crashing
    process/pid), then `fork_verify: AV-path stale rip livelock detected (same rip repeated), falling
    through to deeper slot healers`, then the fatal signal -- i.e. `fork_verify`'s own THREAD-based
    stale-pointer single-step healer is engaging and failing to resolve the fault. Per this file's
    own standing description of cross-process fork ("a genuine `D == 0` fork... no relocation, no
    `fork_verify` healing"), this machinery should not even be relevant here -- strongly consistent
    with the 84th pass's own flagged-but-never-confirmed "interaction with `fork_verify.rs`'s own
    watched-code-page machinery" hypothesis for this exact bug class, now with concrete rip/fault_addr
    evidence instead of a guess. **Confirmed absent from the non-lazy baseline** (0 `fatal signal`
    lines in `.wfgy/pass103_xset_repro_nolazy.log`, vs. 48 in this pass's fixed-but-still-lazy run) --
    genuinely caused by the lazy path, not pre-existing/unrelated noise. Root cause NOT found this
    pass -- per this file's own standing practice for this bug class, the next step is a live
    `cdb -pv` attach (debug build) on `fork_verify::on_single_step`/`vectored_exception_handler`
    breaking on the exact `rip=0x110097b30` repeat, not further code reading. **Both lazy flags stay
    default OFF; still not safe for a real boot.** No full real-boot attempt made this pass (the
    cheap repro alone already disqualifies the flags; spending a ~3+ minute boot cycle on a
    known-still-broken config would not have added information). `DE_UP` not attempted.
  - Logs: `.wfgy/pass104_xset_fix.{out,err,poll}.log` (guard-cow on), `.wfgy/pass104_xset_fix2.
    {out,err}.log` (guard-cow off, the 48-crash/`rc=139` evidence).
- **105th -- root-caused+FIXED a SECOND, independent sigreturn-trampoline livelock, this time in
  `fork_verify.rs` itself (not `lazy_commit_veh`); corrects the 104th pass's own "constant
  `rip=0x110097b30`" characterization -- that address was specific to that pass's own run and is NOT
  a fixed/meaningful constant across builds. Found via log evidence + call-graph reading (not a live
  `cdb` attach), verified via real rebuild-and-rerun A/B, not fabricated. Still does not close the
  repro: a THIRD, distinct, uncharacterized crash remains.**
  - **Root cause**: `run_thread_with_fork_verification` (`litebox_platform_windows_userland/src/lib.rs`,
    pass 142/143) arms `fork_verify::begin` for EVERY real `LITEBOX_PROCESS_FORK=1` cross-process fork
    child, not just the old thread-based fallback path -- a deliberate, general safety net "for
    whatever the group-relocations copy doesn't cover" per that function's own doc comment. It is
    handed an IDENTITY `AddressRelocations` (`relocations.is_identity()` true, source == destination
    by construction for a cross-process child). Under an identity map, `relocations.is_in_source(rip)`
    is true for essentially every address the child legitimately executes -- including the sigreturn
    trampoline's own deliberately-permanent-fault page. `fork_verify.rs` had ZERO references to
    "sigreturn" anywhere in its ~2850 lines before this pass, so both `on_single_step`'s case (1) and
    its AV-path counterpart `translate_stale_source_rip` "healed" this fault too: `relocations.
    translate(rip)` on an identity map returns the SAME address, so the retried fetch faults again
    identically forever -- confirmed live via `.wfgy/pass105_xset_repro_lazy.err.log` (built from
    HEAD `67345a4`, which already carries the 104th pass's `lazy_commit_veh` fix):
    `rip=translated_rip=140668768808960` (`0x7feffffef000`, the exact same trampoline address the
    104th pass fixed in the OTHER module) repeating via `fork_verify: stale CODE pointer detected via
    raw access violation`, then `fork_verify: AV-path stale rip livelock detected (same rip
    repeated), falling through to deeper slot healers`, then `fatal signal: terminating task
    signal=Signal(11)`. This also corrects the 104th pass's own framing: the 84th pass's flagged
    "interaction with `fork_verify.rs`'s own watched-code-page machinery" hypothesis referred to the
    `codewatch` diagnostic module (gated `LITEBOX_CODEWATCH=1`, off by default, unrelated to any
    default run) -- not what is actually happening here. The real mechanism is simpler: pass 142/143's
    deliberate, correct wiring of `fork_verify` into the cross-process path just never got the same
    trampoline exclusion `lazy_commit_veh` got in the 104th pass, because the two healers are fully
    independent modules that happen to share the identical failure shape against the identical
    address.
  - **Fix** (`litebox/src/platform/mod.rs`, `litebox_shim_linux/src/syscalls/process.rs`,
    `litebox_platform_windows_userland/src/{lib.rs,fork_verify.rs,process_fork.rs}`,
    `litebox_runner_linux_on_windows_userland/src/lib.rs`): `ForkChildVerificationProvider::
    begin_fork_child_verification` gains a `sigreturn_trampoline: usize` parameter (mirroring
    `spawn_cross_process_fork_child`'s own parameter of the same name/purpose); its one caller
    (`litebox_shim_linux`'s thread-based-fork dispatch) passes `self.sigreturn_trampoline_addr()`;
    `run_thread_with_fork_verification`/`run_thread_inner` thread the SAME value the runner's
    cross-process fork-child bootstrap already parses (`FORK_CHILD_SIGRETURN_TRAMPOLINE_ENV_VAR`,
    the 84th pass's own Bug 2 plumbing, reused a third time) into `fork_verify::begin`. `fork_verify.rs`
    gained a new per-thread `FORK_VERIFY_SIGRETURN_TRAMPOLINE` cell (stamped by `begin()`, cleared by
    `end()`, mirroring the existing `FORK_VERIFY_EPOCH` pattern) and an `is_sigreturn_trampoline()`
    guard applied at both `is_in_source(rip)` gates that can heal a code pointer -- `on_single_step`
    case (1) and the AV-path `translate_stale_source_rip` -- declining (falling through / returning
    `None`) instead of healing, letting the fault reach the normal unhandled-AV dispatch so
    `LinuxShimEntrypoints::exception()` gets the chance to recognize it, exactly mirroring
    `lazy_commit_veh`'s own `EXCEPTION_CONTINUE_SEARCH` decline. The diagnostic-only cross-process
    resume probe (`process_fork.rs:296`) passes a literal `0` (no real trampoline is ever transmitted
    over that synthetic channel).
  - **Verified live, real A/B, on a rebuilt release binary** (`cargo build -p
    litebox_runner_linux_on_windows_userland --release`, confirmed fresh mtime), same
    `.wfgy/pass105_xset_repro.sh` (a byte-for-byte copy of the 103rd pass's `Xvfb`+`xset q` script, fed
    via stdin -- NOT via a `-ArgumentList` inline string, which corrupts on embedded double quotes per
    this file's own PowerShell-quoting caution; see `.wfgy/pass105_xset_repro_lazy.ps1`):
    - Before fix: 20 occurrences of `rip=translated_rip=140668768808960`, "AV-path stale rip livelock
      detected", 6 fatal `Signal(11)` events, `xset rc=139`.
    - After fix: 0 occurrences of that address or livelock message anywhere in the log.
    - `LITEBOX_PROCESS_FORK=1` alone (both lazy flags unset, `.wfgy/pass105_xset_repro_nolazy.*.log`):
      0 fatal signals, `xset rc=0` -- confirms the new plumbing is a genuine no-op on the thread-based
      path (its destination ranges are disjoint from source ranges by construction, so
      `is_in_source(sigreturn_trampoline_dest_addr)` was never true there to begin with).
    - `cargo check --workspace` and `cargo test -p litebox_platform_windows_userland --lib` each have
      one pre-existing, unrelated failure (seccompiler libc symbols in `litebox_platform_linux_userland`;
      a `RawMutex` test-fixture missing-fields error) -- confirmed via `git stash` to be identical on
      pristine `67345a4`, not introduced by this pass.
  - **NOT closed: the SAME repro, same fix applied, STILL shows 6 fatal SIGSEGVs and `xset rc=139`
    with both lazy flags on (0 with `LITEBOX_PROCESS_FORK=1` alone) -- a genuinely THIRD, distinct,
    still-uncharacterized bug.** Zero `0x7feffffef000` hits anywhere in the post-fix run, so this is
    not another instance of the trampoline pattern. New evidence this pass: the clearest instance
    (pid=12020) crashes with `Signal(11)` immediately after `DIAG_TIMELINE execve pid=12020
    ppid=12020 ... argv0=/usr/bin/rm`, preceded by a lone `[diag-recover-fsbase] recover_rip=0x...
    fsbase=0x7feffffb1740` line -- an FS_BASE-recovery event firing right at the guest's post-`execve`
    resume, not another `is_in_source`/livelock signature. Leading untested hypotheses for the next
    pass: (a) an interaction between the very-high-volume, near-every-instruction single-step tracing
    `fork_verify`'s identity-map safety net imposes during the whole fork-to-execve window (tens of
    thousands of "stale CODE pointer... rip==translated_rip" no-op heals observed per fork in this
    pass's own logs) and guard-cow's own PARENT-side write-fault VEH; (b) a genuine FS_BASE-recovery
    race specific to a lazily-committed (not-yet-faulted-in) group being touched for the first time
    right around `execve`. Root cause NOT found this pass. Per this file's own standing practice for
    this bug class, the next concrete step is a live `cdb -p` attach (debug build; invasive `-p`, not
    `-pv`, per the 93rd pass's own correction) breaking on the FS_BASE-recovery path and on
    `execve`'s own guest-resume point for `/usr/bin/rm`, not further log-based diagnosis alone --
    this pass deliberately used log evidence + code reading (faster to land a real, verified fix for
    the FIRST bug it found) rather than a debugger session, and that method has now run out of new
    signal for this THIRD bug's own root cause.
  - Logs: `.wfgy/pass105_xset_repro.sh` (the repro script, byte-identical to the 103rd pass's),
    `.wfgy/pass105_xset_repro_lazy.ps1`/`.err.log`/`.out.log`/`.poll.log` (before-fix evidence),
    `.wfgy/pass105_xset_repro_lazy_fixed.{err,out,poll}.log` (after-fix, 0 `0x7feffffef000` hits),
    `.wfgy/pass105_xset_repro_nolazy.ps1`/`.{err,out,poll}.log` (control, `LITEBOX_PROCESS_FORK=1`
    alone, 0 fatal signals both before and after this pass's fix).
Fully DONE (kept only as a marker so a future pass doesn't re-attempt): the minimal isolated
cross-process AF_UNIX repro; the `Network` shared-arena redesign's `socket_set`/
`LocalPortAllocator`/`closing_in_background`/`queued_for_closure` slice; DISPLAY/`getenv()` as the
`DE_FAILED` cause; AF_UNIX `connect()` `EAGAIN`-vs-`EINPROGRESS`; `pty_registry`/
`daemon_pty_masters` (`syscalls::pty::SharedPtyTable`, live-verified cross-process); fork's
fd-eligibility scan dropping a redirected 0/1/2 (`raw_fd_is_plain_stdio_device`);
`SharedUnixConnectQueue`'s cancel-on-first-non-blocking-miss gap (`UnixStreamState::Connecting`);
both Xvfb SIGSEGVs.

**Open, in rough priority order:**

1. **`DE_FAILED` is root-caused on BOTH configurations (103rd); the lazy-fork config's now had TWO
   independent sigreturn-trampoline sub-bugs FIXED (104th: `lazy_commit_veh`; 105th: `fork_verify.rs`
   itself), but a THIRD, distinct bug in the same config remains OPEN and still blocks it, and the
   non-lazy config's RAM-crater blocker is untouched.**
   - **Lazy-fork config**: both the 103rd pass's `0x7feffffef000` crash in `lazy_commit_veh` (104th)
     AND the 105th pass's independent discovery of the SAME address/pattern in `fork_verify.rs`'s own
     cross-process safety net (pass 142/143's `run_thread_with_fork_verification`, armed with an
     IDENTITY relocation map for every cross-process child) are FIXED and live-verified -- zero
     `0x7feffffef000` occurrences anywhere in the post-105th-pass repro, see that pass-history entry
     for the full mechanism and fix. **This corrects the 104th pass's own "constant `rip=0x110097b30`"
     framing and its "84th pass `codewatch` interaction" citation** -- that address was specific to
     that one run (not a build-stable constant), and `codewatch` is an always-off-by-default
     diagnostic module unrelated to this bug; the real mechanism was `fork_verify`'s OWN cross-process
     wiring never having the trampoline exclusion `lazy_commit_veh` got in the 104th pass. **Still NOT
     safe for a real boot**: the SAME repro, both fixes applied, STILL produces 6 `fatal signal:
     terminating task signal=Signal(11)` events and `xset` itself still crashes (`rc=139`) -- a THIRD,
     genuinely distinct bug (zero `0x7feffffef000` hits in this run, so not a third instance of the
     trampoline pattern). New evidence (105th): the clearest instance crashes immediately after
     `DIAG_TIMELINE execve ... argv0=/usr/bin/rm`, preceded by a lone `[diag-recover-fsbase]
     recover_rip=... fsbase=...` line -- an FS_BASE-recovery event at the guest's post-`execve`
     resume, confirmed absent with `LITEBOX_PROCESS_FORK=1` alone (0 fatal signals in that control).
     `LITEBOX_LAZY_FORK_COMMIT`/`LITEBOX_LAZY_FORK_GUARD_COW` must NOT be recommended or used for any
     real boot attempt until THIS is fixed. Cheapest known repro (still applies, now piped via stdin
     to avoid PowerShell `-ArgumentList` double-quote corruption -- see the 105th pass's own
     `.wfgy/pass105_xset_repro_lazy.ps1`): `.wfgy/pass105_xset_repro.sh` (`Xvfb` + `xset q`, no DE at
     all, byte-identical to the 103rd pass's own script) under `LITEBOX_PROCESS_FORK=1
     LITEBOX_LAZY_FORK_COMMIT=1 LITEBOX_LAZY_FORK_GUARD_COW=1`. Next pickup, precise: a live
     `cdb -p` attach (debug build; invasive `-p`, not `-pv`, per the 93rd pass's own correction),
     breaking on the FS_BASE-recovery path and on `/usr/bin/rm`'s own post-`execve` guest-resume
     point -- the 105th pass deliberately used log evidence + code reading rather than a debugger
     session (faster, and sufficient to land the first, verified fix), but that method has run out of
     new signal for this third bug's own root cause. Two untested hypotheses to check first: (a)
     interaction between `fork_verify`'s now-confirmed very-high-volume identity-map single-step
     tracing during the whole fork-to-execve window and guard-cow's own parent-side write-fault VEH;
     (b) a genuine FS_BASE-recovery race specific to a lazily-committed (not-yet-faulted-in) group
     touched for the first time right around `execve`. Do NOT retune `live_cross_process_fork_
     children`'s admission cap or `GUARD_COW_CONCURRENT_CLAIM_CAP` in response -- this is a
     correctness bug neither capacity lever can fix or should mask.
   - **Non-lazy config (`LITEBOX_PROCESS_FORK=1` alone, the recommended safe path)**: `DE_FAILED` ==
     the pre-83rd-pass RAM crater (Track B item 1) cutting the boot off before `xfwm4` (which DOES
     launch and survive, confirmed 103rd on the current binary) finishes initializing --
     `_NET_SUPPORTING_WM_CHECK` is never set because the whole process tree dies first, not because
     `xfwm4` crashes or is logically stuck. **New, unconfirmed lead**: `xfwm4` spent its entire
     observed lifetime in both 103rd-pass runs repeatedly hitting the item-2 cross-process AF_UNIX
     `ECONNREFUSED` gap below -- possibly why it's too slow to beat the crater. Next pickup: confirm
     directly whether `xfwm4` ever gets past this retry loop, then decide whether fixing item 2 or
     continuing the original RAM-crater angle (empirically tuning the admission caps, or the still-
     unexplored genuine per-page lazy population once the config above is actually safe) is the
     better lever for THIS blocker specifically.
   - Full evidence for both: 103rd-pass entry above. Full evidence for 98th-102nd (the
     `GUARD_COW_CONCURRENT_CLAIM_CAP` crater-speed fix, Bugs 6a/6b/7): pass-history section above and
     `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "98th-100th pass full narrative".
2. `SharedUnixConnectQueue`'s cancel-on-claim-race slot leak — FIXED 62nd (`unix.rs`); didn't
   resolve item 1's symptom. Other AF_UNIX exhaustion paths still silent (38th, `unix.rs`):
   `SharedUnixAddrPresenceTable` capacity-256 overflow; a key >108 bytes; backlog ignored on
   cross-process accept. Abstract sockets CORRECT. **Possibly directly relevant to item 1's non-lazy
   `DE_FAILED` blocker now (103rd)**: `unix_addr_table`'s Backlog/Channel values not being
   shared-memory-native is what produces the `[unix_addr_presence] ECONNREFUSED but address IS
   bound, by a DIFFERENT guest pid` warning `xfwm4` hit repeatedly, with no other visible progress,
   in both of the 103rd pass's own real non-lazy boots -- see item 1's own new-lead note.
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
