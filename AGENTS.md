# litebox -- current state (2026-09-24)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, or one a later commit superseded, is deleted rather than hedged. Reference
detail is drained to `docs/AGENTS_ARCHIVE_*.md` and dated `docs/*.md` in the map below -- read those
for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one
line plus its pointer, not a separate memory file. Compacted at the 65th, 70th, 72nd, 75th, 76th,
81st, 83rd, 85th, 88th, 91st, 93rd, 95th, 98th, 101st, 104th and 106th passes (pass-history section
below; 26th-69th full narrative: `docs/AGENTS_ARCHIVE_2026-09-22.md`; 70th-105th full narrative,
including each pass's own complete evidence and fix rationale: `docs/AGENTS_ARCHIVE_2026-09-23.md` --
every condensed bullet below cites this same archive for its own full narrative). The 106th pass's
compaction took this from 74.3KB to ~54KB by draining the 103rd-105th passes' own full narrative to
the archive -- still over the 30KB target; "Standing lessons"/"Docs and tooling map"/"Closed" are the
remaining drain candidates for a future pass (the pass-history bullets above this line are already
condensed close to the point of losing load-bearing detail; further cuts there risk exactly the
"claim nobody could point at" failure mode this file's own opening paragraph warns against).

**Where things stand, in one paragraph (updated 113th pass; comm-inheritance fix and new negative
evidence added by the 114th pass, see its own pass-history entry below)**: three real gaps that used to
force every cross-process fork onto the crash-prone thread-based relocating path are now closed --
109th-112th passes landed cross-process `kill()`/process-groups/`SIGCHLD` siginfo, a single shared
data path for ptys (Ctrl-C, `script`, interactive terminals all verified), and unix-socket carrying
(connected streams, socketpairs, listeners) -- real boots now show **zero** `not eligible` fallbacks
(`.wfgy/pass112_boot{1,2,3}.err.log`). The 103rd pass's `xfce4-session: Cannot open display: .` +
immediate `exit(1)` **is a RACE, not a deterministic bug** (envp proven correct at `execve` via a
new permanent diagnostic, `[diag-xfce4session-envp]`), and it is CLOSED for the non-lazy path: two
harness bugs caused it -- `de_only.sh`'s `XSOCK_WAIT_DONE` only checked the socket FILE's existence,
not Xvfb's actual connection-accepting readiness (fixed: `PROBE_XSET` now retries its own connect
probe up to 15s); and `xrdb "$HOME/.Xresources"` ran through xrdb's default cpp-preprocessing pass
for a one-line file with nothing to preprocess, forking a real `sh -> cpp -> cc1` chain that
directly caused a live ENOMEM crash (fixed: `xrdb -nocpp`). Both fixes are baked into a new seed,
`.wfgy/pass113_de_only_ready_seed.tar` (built from `pass103_de_only_trimmed_seed.tar` -- use this
one, not the older `_trimmed_` seed, for all future boots). **With both fixes, non-lazy
(`LITEBOX_PROCESS_FORK=1` alone) is now CORRECTNESS-CLEAN END TO END**: `xfce4-session` forks
`ssh-agent`/`iceauth`/`xfwm4`/`xfsettingsd` with zero crashes across every run this pass, confirmed
by a real syscall-level trace (`.wfgy/pass113_sshagent_hang2.err.log`) showing `ssh-agent` exit
cleanly and `xfwm4`'s own `execve` succeed (`.wfgy/pass113_nocpp_boot1.err.log`,
`pass113_final_denup.err.log`). **The sole remaining blocker on the non-lazy path is Track B item
1's own RAM crater** -- every run so far reaches `WM_POLL n=3`-`n=5` (~115-130s) before free RAM
falls below the safety kill-switch (15-17 concurrent processes), just short of `xfwm4` finishing its
own startup long enough to set `_NET_SUPPORTING_WM_CHECK`. This is now purely a resource/timing
question, not a correctness one: a genuinely sustained 6GB+-free run (no large unrelated host
process competing) is very plausibly enough on its own. **`LITEBOX_LAZY_FORK_COMMIT=1
LITEBOX_LAZY_FORK_GUARD_COW=1` (the mechanism that actually avoids the RAM crater) DOES avoid it --
confirmed live, `.wfgy/pass113_lazy_final.out.log` ran the full 200s `WM_POLL` window (n=1..20) with
RAM stable and never cratering -- but has a REAL, DISTINCT, STILL-OPEN correctness bug**, found for
the first time in a genuine desktop-boot shape rather than a synthetic repro: `xfce4-session`'s own
fork of its next session client (right after `ssh-agent` exits and is `wait4`'d) aborts with a
`SIGABRT` before ever reaching `execve` -- guest pid 51 / winpid 2132, comm still blank at the
moment of death, `fatal signal: terminating task signal=Signal(6)`, ~0.17s after "entering real
guest execution" (`.wfgy/pass113_sshagent_hang.err.log:16232`, `pass113_lazy_final.err.log`). This
is the SAME TOCTOU/Bug-4 correctness class `lazy_fork_commit.rs`'s own doc comment has documented
since the 85th pass (a lazy-serviced page reading the parent's CURRENT, not fork-time, memory) --
the concurrent-claim-cap mitigation (102nd pass) reduces its FREQUENCY but was never proven to
CLOSE it, and this is the first time it has actually recurred in the real target workload rather
than a synthetic subshell. **`DE_UP` has not been reached by any of the 113 passes to date. Both
lazy-fork flags remain default OFF.** Recorded as tracked defects in `.gm/prd.yml`:
`non-lazy-fork-ram-crater-before-de-up` (resource/timing, non-lazy) and the pre-existing
lazy-fork-commit TOCTOU item (rescoped to include this new real-workload repro). **Next pickup**:
(a) non-lazy -- **corrected, live-measured**: 6GB free at boot start is NOT enough on its own --
two independent runs starting at 6.09-6.36GB free both cratered at the identical `WM_POLL n=4`
point (~130s elapsed, 14->15->17 processes, 0.6-0.8GB free) with zero correctness issue either
time. The boot's own cumulative committed-memory need by this point is a real, consistent ~5GB+
regardless of starting headroom in this range -- needs either more like 8-10GB+ free sustained, or
a genuine reduction in Track B item 1's own cumulative cost (the original, still-open 76th-82nd
pass investigation). This needs no further code changes, only more host RAM than has been
available this session. (b) lazy -- **negative evidence gathered, real trigger still not isolated
to a cheap repro**: neither a sequential fork-no-exec/`wait4`/fork-no-exec-again pattern nor a
genuinely CONCURRENT two-outstanding-children pattern (both on `debian:stable-slim`,
`.wfgy/pass113_sigabrt_repro.sh`/`pass113_concurrent_repro.sh`) reproduces the crash -- both ran
clean, no `Signal(6)`, both children exited normally. The real trigger likely needs the SPECIFIC
shape only the real boot has: `xfce4-session`'s own vfork'd `/bin/sh`->`iceauth` chain (shares the
SAME Windows process as `xfce4-session` itself, unlike an ordinary cross-process sibling) still
alive/untracked-as-reaped at the moment a THIRD, genuinely cross-process child is forked -- **this
hypothesis is now REFUTED by careful re-reading, not just untested**: real vfork semantics (and
this codebase's own implementation, `Process::wait_for_vfork_done`) block the PARENT's own thread
entirely until the vfork child calls `execve`/exits, and `detach_pm_for_vfork_execve` gives the
`execve`'ing child (`iceauth`) a brand-new, fully-detached `PageManager` at the START of its own
`sys_execve`, before touching memory -- so by the time `xfce4-session`'s thread is even running
again (to fork the crashing child), the vfork sharing has ALREADY ended and nothing else has any
claim on its memory. Drop this angle; it does not explain the crash.
  - **Real, general (non-crash-specific) gap found while investigating, ROOT-CAUSED AND FIXED
    THIS PASS (114th)**: the 113th pass's own claim that "`comm` is NEVER copied from the parent at
    fork time" was too general and is corrected here -- `do_clone`'s real thread-based clone path
    (`litebox_shim_linux/src/syscalls/process.rs:5103`) already did this correctly
    (`comm: self.comm.clone()`, with an explicit doc comment at line 5148 confirming real-Linux
    semantics). The actual, narrower bug was specific to the OTHER Task-construction path: a
    cross-process fork child's own `Task` is built fresh, with no parent `Task` in the same OS
    process to copy from, by `LinuxShim::adopt_forked_process`
    (`litebox_shim_linux/src/lib.rs:1062`) -- and THAT function unconditionally hardcoded
    `comm: [0; TASK_COMM_LEN].into()`, discarding whatever the parent was actually named, on EVERY
    cross-process fork (i.e. every fork under the recommended `LITEBOX_PROCESS_FORK=1` path -- the
    dominant one in this whole 113-pass investigation). This is exactly why every `DIAG_TIMELINE
    clone`/`execve` line for a forked child showed a blank `comm`, and exactly why
    `LITEBOX_DIAG_SYSCALL_TIMELINE`'s comm-based filter could never see a forked child's own
    pre-`execve` syscalls (the blind spot that made the 113th pass's "zero syscalls ever traced for
    the crashing pid" finding meaningless). **Fixed by threading the parent's real `comm` bytes
    through the same way `sigreturn_trampoline` already was** (same precedent, same
    `CreateProcessW`-environment-variable-export mechanism): added `comm: [u8; 16]` to
    `litebox::platform::PlatformExtensions::spawn_cross_process_fork_child`'s trait signature
    (`litebox/src/platform/mod.rs`), threaded it through the Windows impl
    (`litebox_platform_windows_userland/src/lib.rs`) and `spawn_process_fork_child`
    (`process_fork.rs`, new `FORK_CHILD_COMM_ENV_VAR` = `LITEBOX_INTERNAL_FORK_CHILD_COMM`,
    hex-encoded to survive the env-var boundary), read it back in the child-side bootstrap
    (`litebox_runner_linux_on_windows_userland/src/lib.rs`'s `diag_process_fork_task_resume_probe`
    -- confirmed, despite its `diag_` name, to be the REAL production Task-construction path for
    every cross-process fork child: `spawn_process_fork_child`'s own doc comment says the three
    `LITEBOX_DIAG_PROCESS_FORK_*` gates it sets are "always" set, unconditionally, on this
    production path), and passed it into `adopt_forked_process`'s now-required `comm` parameter.
    **Live-verified** (`.wfgy/pass114_comm_repro.sh`/`.ps1`, cheap `debian:stable-slim` + `/bin/bash
    -s` repro, `LITEBOX_PROCESS_FORK=1` alone, no lazy flags): `DIAG_TIMELINE execve pid=2 ppid=1
    comm=[98, 97, 115, 104, 0, ...] argv0=/usr/bin/sleep` -- `[98,97,115,104]` decodes to `"bash"`,
    the forked child's real parent name, exactly where a blank `[0,0,...]` array appeared before
    this fix. Added the pid-based `LITEBOX_DIAG_SYSCALL_TIMELINE_PID` filter (113th pass) remains
    useful as a belt-and-suspenders option, but the comm-based filter itself is now trustworthy
    again for cross-process-forked children too, not just thread-based ones. Not yet re-attempted
    against the actual lazy-fork-commit SIGABRT crash (guest pid 51/winpid 2132) -- that is the
    immediate next pickup now that the tooling blind spot is genuinely closed rather than routed
    around.
  - **Also tried and correctly abandoned this pass**: `LITEBOX_DIAG_PROCESS_FORK_EXTERNAL_DEBUGGER`
    (a pre-existing, 143rd/144th-pass kernel-debug-event observer) plus a new
    `LITEBOX_DIAG_PROCESS_FORK_EXTERNAL_DEBUGGER_SKIP=<n>` gate added this pass to scope it to only
    the fork under investigation. It correctly passes exceptions through to VEH
    (`DBG_EXCEPTION_NOT_HANDLED`, confirmed by re-reading `observe_real_resume_fault` -- an EARLIER
    draft of this same paragraph wrongly claimed it bypasses VEH via `DBG_CONTINUE`; that claim was
    corrected in the same pass, `162fe02`, rather than left standing), but its own real per-event
    overhead (a `WaitForDebugEvent` round trip, `GetThreadContext`, a multi-field `eprintln!`, per
    fault) was enough to measurably worsen the RAM crater on a lazy-fork-commit boot (which can
    legitimately take many deliberate page faults during ordinary startup) -- confirmed by direct
    comparison against the clean, undiagnosed `pass113_lazy_final` run. Kept the `_SKIP` gate
    (harmless when unset, real use for a genuinely unrecoverable non-lazy crash where this overhead
    doesn't matter), but it is not the right tool for THIS specific investigation.
  - **Next pickup, precise**: once host RAM is genuinely, sustainedly free (the 8-10GB already
    established as insufficient at 6GB doesn't even apply here -- this pass never even reached 5GB
    sustained), re-run targeting the crashing lineage's own comm (now genuinely trustworthy across
    the fork boundary post-114th-pass-fix -- e.g. `LITEBOX_DIAG_SYSCALL_TIMELINE=<comm>`, the
    ORIGINAL, simpler mechanism, not just the pid-based workaround) to finally see the crashing
    child's OWN pre-`execve` syscalls, which no capture has ever shown before. The pid-based
    fallback (`LITEBOX_DIAG_SYSCALL_TIMELINE_PID=48,49,50,51,52,53` or whatever pids a fresh
    `DIAG_TIMELINE clone`/`execve` sequence shows for `xfce4-session`'s own children on that specific
    run, `.wfgy/pass113_pidtrace.ps1`) remains available as a belt-and-suspenders cross-check.

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
- **Cross-process `kill()` goes through `GlobalState::process_table`** (`syscalls/signal/xproc.rs`,
  110th) — every guest process is registered by guest pid -> host pid/pgid/pending-bitmask; a
  remote target gets its bit set + its host's named event (`Local\litebox-sigwake-<hostpid>`) set,
  and that host's listener thread drains into `shared_pending` + `interrupt_all_threads()`.
  `SIGKILL` to a process that owns its host process = `TerminateProcess` with the encoded
  `WIFSIGNALED(9)` exit code. Stop/continue get no special cross-process semantics.
- **A `socketpair(2)`-originated fd (both ends `Unnamed`) is NOT safe to drop as CLOEXEC across a
  cross-process fork** — real processes (`dbus-daemon`'s babysitter) use it for pre-`exec()`
  bookkeeping; `raw_fd_is_addressless_unix_socket_pair` (`net.rs`) makes it CARRIED (112th; it
  used to refuse the fork), while other CLOEXEC unix sockets stay dropped (54th).
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
  process_timeline=debug` for cheap whole-boot coverage. A cross-process fork child's guest pid is
  the parent-allocated `child_tid` (== the parent's `fork()` return/`$!`, 110th; it used to be the
  child's Windows PID, disagreeing with the parent) — the Windows PID is `winpid=` on the child's
  `[process_fork_diag] task-resume-probe` line, and `host_pid` in its `process_table` slot.
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
plus the per-fork fd-kind scan; since the 112th pass no fd kind blocks a real `debian-xfce` boot
(0 `not eligible` in 3 boots; was 11-18). Fork-child GPR/vmem-adopt cost is small (~1.2s, down from ~3.5-5s); the rootfs
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

**Pass history (4th-107th, 2026-09-17/23)**: full narrative for every pass below is in the dated
archives ("Docs and tooling map" below) — these bullets are already condensed; do not re-condense
further without re-reading the archive first (the "claim nobody could point at" risk this file's own
opening paragraph warns about).

- **4th-42nd**: `docs/AGENTS_ARCHIVE_2026-09-22.md`. Fork fd eligibility, OCI cache, s6-boot,
  cross-process fork's initial `D==0` design and its first bug wave (guest-mmap alignment, stdio
  handles, `fd/mod.rs:422`).
- **43rd-74th**: `docs/AGENTS_ARCHIVE_2026-09-22.md`/`_2026-09-23.md`. FIXED/REFUTED, live-verified:
  both Xvfb SIGSEGVs; D-Bus activation's dropped-CLOEXEC-fd bug; per-fork rootfs-rebuild RAM cost
  (56th); `ssh-agent`/`xfwm4` permanent freeze (`RawMutex::WaiterQueue::with_lock`, 60th/61st);
  `SharedUnixConnectQueue::cancel` slot leak (62nd); `DBUS_FAILED` (67th/68th). REFUTED: `/defaults/
  xfce/` readdir, dbus babysitter SIGKILL, epoll-readiness, GLX/compositor theories. 70th-74th:
  root-caused two logging gaps hiding `xfwm4`'s own X11 traffic; added `litebox_diag::
  process_timeline`/`socket_read`. `DE_FAILED`/RAM collapse survived all of it.
- **75th-82nd**: `docs/AGENTS_ARCHIVE_2026-09-23.md`. 75th: **`xfwm4` launches for the first time
  ever**, FIXED (`1d449e6`) a writable-layer export-path bug breaking filesystem-write visibility on
  every boot. 76th: admission control (`live_cross_process_fork_children`, caps 6) — real but
  partial. 77th: FIXED (`621ee1a`) a real 2x host-allocator commit-doubling bug — not sufficient
  alone. 78th-82nd: measured (not guessed) that fork-then-immediate-`execve()` wastes ~85% of cycle
  time on eager copy, ruled out cap-tuning and two deferred-copy shortcuts (a second correctness
  obstacle: a plain `fork()` child may legally write memory pre-`execve`), converging on genuine
  per-page lazy population as the only remaining lever.
- **83rd-87th**: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "83rd-87th pass full narrative". Implemented
  lazy reserve-then-commit-on-fault fork memory (`lazy_fork_commit.rs`,
  `LITEBOX_LAZY_FORK_COMMIT=1`) — real win for fork-then-`execve`; fixed 3 bugs along the way (guest-
  mmap alignment collision, sigreturn-trampoline non-inheritance, an active-`%rsp`-group bug); found
  but did NOT fix Bug 4 (TOCTOU: a lazily-serviced fault reads the parent's CURRENT memory, unsafe
  for fork-without-`execve`); designed the single-generation guard-page-COW fix. Both flags default
  OFF throughout; `DE_UP` not attempted.
- **88th-90th**: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "88th-90th pass full narrative". 88th:
  IMPLEMENTED guard-page COW (`LITEBOX_LAZY_FORK_GUARD_COW=1`), fixed a guard-cow hang (Bug 5),
  reached furthest yet (`WM_POLL` → a real X window) before `DE_FAILED`. 89th: root-caused
  `DE_FAILED` to a live `xfce4-session` `STATUS_ACCESS_VIOLATION` crash, unrelated to lazy/guard-cow.
  90th: root-caused it to a genuine `CLONE_VFORK`, fixed two architectural bugs (blind fresh
  `PageManager` risking live-parent-memory corruption on `execve`; a `release_memory`/
  `Vmem::duplicate` regression that fix itself caused) — both live-verified fixed, but the ORIGINAL
  crash itself survived unchanged into the 91st pass.
- **91st-97th**: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "91st-94th"/"95th-97th pass full narrative" —
  **a 7-pass misattribution, resolved.** 91st-96th chased a "Windows clears `GS_BASE`/`FS_BASE` under
  scheduling pressure" theory (fixed several real, general `RawMutex` register-repair gaps along the
  way; kept landed) to a decisive negative result. **97th overturned the theory entirely**: the real
  crash is a plain `CLONE_THREAD` pthread hit by the already-documented Bug 4 TOCTOU, confirmed via a
  clean A/B — the whole FS_BASE/GS_BASE chase, while each fix is real, was chasing a misattributed
  symptom of Bug 4. Also fixed: `VEH_FRAME_STRIDE`/`EXCEPTION_RECORD_RESERVE` sized only for release
  codegen, crashing every debug build — widened 8x under `#[cfg(debug_assertions)]`.
- **98th-101st**: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "98th-100th pass full narrative". 98th
  generalized Bug 4's fix to N concurrent per-parent generations (`GUARD_PAGE_REGISTRY`). 99th found
  a real 100%-reproducible regression (crater at 20s/7 procs; new `rc=134` heap corruption). 100th
  root-caused `rc=134` to Bug 7 (`sys_execve` never called `disarm_on_execve`) and fixed it plus two
  more (Bug 6a: `OpenProcess` handle churn; Bug 6b: `GUARD_PAGE_REGISTRY`/`mprotect` desync) — `rc=134`
  gone, but an older `rc=139` SIGSEGV resurfaced, crater speed unchanged. 101st implemented the
  batched-`VirtualProtect` optimization (real win, kept landed) but confirmed by A/B it does NOT fix
  crater speed (the crater is `VirtualAlloc2(MEM_COMMIT)` charge, not guard-cow overhead) — deferred
  `rc=139` to a live `cdb` session. Both flags default OFF throughout; `DE_UP` not reached.
- **102nd**: root-caused+FIXED the crater-speed regression (99th-101st) by code reading: every
  guard-cow claim `Box::leak`s a full-page table, and 98th's N-concurrent generalization removed an
  incidental rate limit the old single-owner gate provided. Fix: `GUARD_COW_CONCURRENT_CLAIM_CAP: u32
  = 3` (`lazy_fork_commit.rs`) restores it. Verified: isolated repro + real boot A/B (crater-to-
  kill-switch → stable ~188s run, matching the pre-98th baseline). `rc=139` reconfirmed real and
  unaffected (not root-caused this pass; superseded by 103rd/104th below).
- **103rd-105th**: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "103rd-105th pass full narrative". 103rd:
  first pass to directly investigate `DE_FAILED` — root-caused it on both configs (lazy:
  `xfce4-session` dies of `STATUS_ACCESS_VIOLATION`; non-lazy: RAM crater); found the cheap
  `Xvfb`+`xset q` repro for the same crash class (17/25 faults at `0x7feffffef000`). 104th: FIXED one
  real cause — the sigreturn-trampoline page could be merged into a lazy-eligible group,
  infinite-refaulting its own deliberate trap (`classify_lazy_eligible_groups` fix) — zero
  `0x7feffffef000` post-fix, but the same repro still showed `Signal(11)`/`rc=139`, traced to
  `fork_verify.rs`'s OWN independent healer hitting the same address. 105th: FIXED that second
  instance (threaded `sigreturn_trampoline_addr()` into `fork_verify.rs`'s heal-decline check) — 20
  occurrences → 0 post-fix. **Still NOT closed**: a THIRD, distinct bug (6 fatal `Signal(11)` events,
  zero trampoline hits) — clearest crash follows `DIAG_TIMELINE execve argv0=/usr/bin/rm`. Both lazy
  flags stay default OFF.
- **106th-107th**: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "106th-107th pass full narrative" (the
  deterministic-address evidence, the refuted CoW-mmap hypothesis, the `allocate_pages` near-miss
  analysis). Investigated the 105th pass's open `/usr/bin/rm` crash (a deterministic guest `#PF` at
  `cr2=0x111156f60`, litebox's `Vmem` believing the page present while Windows backing was not) by
  code reading and log re-mining, ruling out `allocate_pages`, the CoW-mmap fast path, and a stale
  VMA-adoption diagnostic filter — did not find the mechanism. **Superseded**: the 109th pass fixed
  this exact crash from a completely different angle (`sys_execve`'s disarm-ordering) — see below.
- **109th-113th (full narrative drained to `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "109th-113th pass
  full narrative" section)** -- closed the three real gaps that used to force every cross-process
  fork onto the crash-prone thread-based path. 109th: fixed the crash the 102nd/106th/107th passes
  chased -- `sys_execve` was disarming `lazy_fork_commit`'s lazy servicing at function ENTRY,
  before `copy_vector` finished reading the OLD program's still-lazy `argv`/`envp` (`24cb72d`); also
  fixed a false-alarm AF_UNIX presence-miss WARN (`d16e5ce`). 110th: cross-process `kill()`/
  process-groups/`SIGCHLD` (`SharedProcessTable`, `xproc.rs`), plus pty `ISIG`/slave-carrying; found
  and fixed a real guest-pid-vs-Windows-pid identity bug along the way. 111th: pty data path
  unified onto `SharedPtyTable` for every process (no more per-process-local channel gap); `SIGCHLD`
  now carries real child siginfo. 112th: unix sockets (connected streams, socketpairs, listeners)
  carried across a cross-process fork -- eliminates the last thread-path fallback; fixed a dead-
  slot-reclaim bug and a writable-layer-import abort-on-first-failure bug along the way. 113th:
  fixed a real diagnostic bug (`is_syscall_timeline_target_comm` matched trivially on an empty
  `comm`, hiding every forked child's pre-`execve` syscalls) and confirmed `xfce4-session`'s
  "Cannot open display: ." is a **timing race**, not a deterministic bug -- same script, same code,
  clean run vs. crash run. **Net result of 109th-113th**: real boots now show zero `not eligible`
  fallbacks, and non-lazy (`LITEBOX_PROCESS_FORK=1` alone) is correctness-clean end to end -- the
  sole remaining blocker on that path is Track B item 1's own RAM crater (below), a resource/timing
  question now, not a correctness one.
- **114th (full narrative drained to `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "114th pass full
  narrative" section)** -- fixed the comm-inheritance bug (top paragraph, committed, live-
  verified). Built a synthetic multi-threaded-parent fork stress test and found a genuine NEW
  host-level `STATUS_ACCESS_VIOLATION` (distinct from the pre-existing guest-level Bug 4 TOCTOU),
  reproducible under `LITEBOX_LAZY_FORK_GUARD_COW=1` in under 2 seconds, no full boot needed
  (`.wfgy/pass114_torn_read_probe.sh`). Refuted three theories in a row by direct rebuild+test
  (per-page guard-install timing; a `__chkstk` stack-overflow, fixed regardless via a
  `thread_local!` scratch buffer, `FAULT_SCRATCH_BUF`; and an INITIALLY-plausible sigreturn-
  trampoline main-handler correlation, refuted once real pid-tagging was added -- pure cross-
  process log interleaving). Installed `cdb` (user-approved) and, after working around several
  `-c`-script-timing/quoting obstacles (full detail: archive), got a live, `!address`-verified
  capture: the crashing GUEST thread's own stack page is `MEM_COMMIT`/`PAGE_READONLY` with
  `Allocation Protect: PAGE_NOACCESS` -- a real, committed page stuck read-only, so its very first
  ordinary write crashes. Theorized mechanism: a stale guard-cow claim's `PAGE_READONLY` from an
  earlier, since-dead fork generation, never healed because nothing outside `guard_one_page`'s own
  per-page claim path ever re-checks a page once it's no longer being actively guarded. Implemented
  `heal_stale_guard_entries_in_range` (called from both `allocate_pages` success paths, real
  hardening, kept) but **live re-test confirmed it does NOT fix this crash** -- no registry entry
  existed for this page at fault time, so the theorized mechanism is wrong or incomplete. Also
  chased and REFUTED a "mystery small-stack internal thread" theory (implemented+tested twice, no
  effect) that was itself based on a red herring: named every litebox-internal background thread
  (`ctxwatch.rs`'s DR1 helper, `net.rs`'s TCP-flow-connect helper, `lib.rs`'s fork-pipe-pump/
  xproc-exit-notifier/signal-wake-listener threads, all now `litebox-*`-named; `spawn_thread` now
  names every GUEST thread `litebox-guest-pid<N>`) and re-captured: the crashing thread is
  `"litebox-guest-pid2"`, an ORDINARY guest thread (its tiny TEB-tracked stack is irrelevant --
  litebox runs guest code with `%rsp` pointing at guest-mapped memory, never the host thread's own
  TEB stack, by design). By end of pass, code reading had ruled out every candidate call site
  found by inspection (`guard_one_page`, `try_guard_region_batched`, `invalidate_guarded_range`,
  `heal_stale_guard_entries_in_range`, `lazy_commit_veh`, `reserve_and_commit`/`prot_flags`) --
  none can produce this exact `PAGE_READONLY` signature for an ordinary thread-stack `mmap`. Also
  confirmed (by direct code reading, not assumption) that a genuine negative finding this pass --
  zero `[lazy_fork_commit]` diagnostic output despite the guard-cow-like signature -- is NOT
  explained by a stdio- or env-inheritance gap for the cross-process-forked child (both verified
  correctly wired); the real explanation remains unconfirmed. **Five consecutive attempts at a
  live `cdb` breakpoint capture on `VirtualProtect`/`VirtualAlloc2` for the crash address each hit
  a genuinely new cdb-scripting obstacle** (`-g`-deferred script timing; an invalid `sxd ibp` event
  name; `&&` not parseable in `.if`; and a `g`-inside-a-`$$<`-loaded-file failure reproduced twice,
  once recursively and once fully unrolled, root mechanism not identified) -- full blow-by-blow in
  the archive. **Declined a sixth blind attempt**: a future session should go interactive (a real
  `cdb` window, not scripted/redirected) or solve the original quoting problem without a `$$<file`.
  **Net status**: crashing thread identity confirmed (ordinary guest thread); root mechanism of
  the `PAGE_READONLY` signature still NOT confirmed after ruling out every code-reading candidate;
  do NOT ship a blind fix; the trampoline-collision loop and this STATUS_ACCESS_VIOLATION remain
  two DIFFERENT, unresolved bugs; `xfce4-session`'s own original SIGABRT remains unre-attempted.
- **115th -- compacted this file (78.8KB -> 53.1KB, full 114th-pass narrative drained to the
  archive); tried a `cdb`-free repro (litebox's own always-reliable `eprintln!` diagnostics,
  `LITEBOX_DIAG_MM=1` + `LITEBOX_DIAG_LAZY_FORK_COMMIT=1`, no debugger at all) to sidestep the
  114th pass's cdb-scripting dead end -- found a new, real, but still-unconfirmed data point,
  not a root cause.** 2/2 plain (non-cdb) runs of the exact same repro end in the guest-visible
  process getting reported as killed by SIGKILL (bash's own `$?`=137 after the `python3 -`
  heredoc) within ~7s, both with `LITEBOX_DIAG_NO_FAULT_WATCHDOG`/`_NO_EXTERNAL_FAULT_WATCHDOG`
  set (ruling out both known internal watchdogs as the cause -- confirmed no third watchdog gate
  exists by grepping every `WATCHDOG`-related env var in the codebase) and with
  `killed_by_ram_switch: False` confirmed by the driving script itself (not an external kill from
  this session's own tooling either). **No crash dump of any kind appears** -- `.wfgy/
  pass115_plain_repro{2,3}.err.log` end abruptly on an ordinary `[lazy_fork_commit]` line with
  nothing after, despite AGENTS.md's own standing claim that "a fatal host fault dumps before it
  dies, ungated... no env var needed." Plausible explanation, not yet confirmed: the crashing
  guest thread's own `%rsp` points at GUEST-mapped memory rather than its real host stack (per
  the 114th pass's own finding), so whatever native stack-walking the crash-dump path relies on
  may find a bogus/inaccessible stack and silently produce nothing for this specific fault class
  -- if true, this is a second, independent bug (a real crash producing no diagnostic at all) worth
  fixing on its own merits regardless of the STATUS_ACCESS_VIOLATION's own root cause, but NOT
  confirmed this pass (declined to chase it further this session; see "Next pickup" below). The
  burst of `[lazy_fork_commit] guard-cow: page=... INVALIDATED` lines immediately preceding each
  SIGKILL report is for a DIFFERENT address range (`0x111148000..0x111168000`) than the cdb-
  confirmed crash address (`0x7feffe490000`), so it is very likely unrelated ordinary teardown
  activity, not the crash mechanism itself -- included here only because it is the last visible
  activity before the process disappears both times, not because a causal link is established.
  **Next pickup**: (a) confirm or refute the "no dump because %rsp is a guest address" theory by
  code-reading the actual crash-dump/stack-walk implementation (search for "dumps before it
  dies"/`RECENT_FAULTS`/`RECOVERY_LOG` in `litebox_platform_windows_userland/src/lib.rs`) --  if
  confirmed, fixing the dump path itself (making it robust to a bogus `%rsp`, e.g. by capturing
  registers/a minidump via `MiniDumpWriteDump` instead of relying on stack unwinding) would give
  every future pass a working crash dump for this and any similar future fault, a durable
  improvement independent of this specific bug; (b) this `cdb`-free repro path (2/2 reproduced,
  ~7s, no RAM pressure, no debugger overhead) is a genuinely cheaper and more reliable repro loop
  than any `cdb`-based one this session found -- prefer it for future iteration once (a) gives it
  a working crash dump to read.
  - **(a) partially done, same pass, by code reading only (not yet live-tested)**: found the real,
    confirmed reason no dump appears, and it is NOT the `%rsp`-is-a-guest-address theory above --
    `write_crash_minidump` (`lib.rs:10451`) has exactly ONE call site (`lib.rs:2246`), and it is
    reached ONLY from the repeated-identical-fault circuit breaker (the same `rip` faulting more
    than `MAX_REPEATED_UNRECOV_AV = 64` times in a row on one thread, `lib.rs:2184-2252`) -- a
    genuinely UNHANDLED, ONE-SHOT access violation (exactly what every STATUS_ACCESS_VIOLATION
    this whole 114th/115th-pass investigation has captured via `cdb` looks like: it happens once
    per thread, never 64 times in a row) never reaches this call at all, regardless of AGENTS.md's
    own older, now-corrected claim that "a fatal host fault dumps before it dies, ungated." This is
    a REAL, general gap, independent of the STATUS_ACCESS_VIOLATION's own unconfirmed root cause: a
    future pass adding a minidump write to the genuine one-shot-unhandled-AV path too (the
    `[diag-unrecov-av]`-printing `else` branch at `lib.rs:2174`, reached when the host exception
    table has no covering entry for the faulting `rip`) would give every future crash of this
    shape a real dump automatically, with no `cdb` needed. **Not implemented this pass**: tracing
    exactly which branch of this ~1300-line function (`lib.rs:876` onward) OUR specific crash
    actually takes before reaching (or bypassing) that `else` branch needs more careful reading
    than this pass had time for -- the plain (non-cdb) repro's own logs show NONE of
    `[diag-unrecov-av]`/`[diag-extable]`'s output either (both described as "ungated,
    allocation-free" in their own comments), meaning our crash is intercepted even EARLIER than
    that branch, by some other part of this function's fault-classification logic, not yet
    identified. Do not add a minidump call to the `else` branch alone without first confirming
    that is actually where this crash's own dispatch goes -- it may need to go somewhere earlier.
- **116th -- the STATUS_ACCESS_VIOLATION chased since the 114th pass is ROOT-CAUSED AND FIXED
  (`702c735`); it supersedes every "mechanism unconfirmed" note above.** A lazy/guard-cow fork
  write-protects lazy-eligible groups in the parent. The exclusion for running threads' stacks was
  fed the SPAWNING thread's `rsp` (`spawn_thread`'s `ctx` still holds the caller's stack pointer;
  clone's `child_stack` is applied later, on the new thread), so a new pthread's own stack became
  `PAGE_READONLY` and its first push faulted with no usable stack. Windows cannot deliver such a
  fault to any VEH/SEH/unhandled-exception filter, which is why no handler, dump or log ever ran and
  the guest saw a bare SIGKILL. It happened BEFORE python called `fork()` (the claim was bash
  forking python; python's threads then started in memory still guarded) -- the 114th/115th
  "guard-cow claim for an earlier fork" and "which thread is it" theories were all downstream of
  this. Fix: new platform hook `note_spawned_guest_thread_stack` (`litebox/src/platform/mod.rs`,
  called from the shim's clone with the real stack top; Windows records it in
  `ALL_THREAD_STACK_RSPS`), and `classify_lazy_eligible_groups` now excludes every group overlapping
  the whole mapping containing a live `rsp`/`rsp-1`. Also added `SetUnhandledExceptionFilter`
  (`last_chance_crash_dump_filter`) so one-shot unhandled faults write a minidump (correct the older
  "dumps before it dies, ungated" claim: only the 64-repeat breaker did; undeliverable faults still
  cannot dump). **Bisect on `.wfgy/pass114_torn_read_probe.sh` (`.wfgy/pass115_plain_repro.ps1`,
  webtop:debian-xfce; no `cdb` needed)**: eager copy (`LITEBOX_PROCESS_FORK=1` only) passes end to
  end in ~10s (`PARENT_DONE`, `RC=0`, 0 torn); lazy-only gives the known guest SIGSEGV; lazy+guard-cow
  now has a correct child (0 torn). Its parent hang was mostly a LOCK-ORDER DEADLOCK I had added
  (`heal_stale_guard_entries_in_range` under `VIRTUAL_PROTECT_LOCK` in `allocate_pages` vs the guard
  VEH's registry-then-protect order; proven from a `cdb -pv` stack dump, symbolized with
  `llvm-symbolizer --relative-address`; removed, `8b982f3`; 13/20 hung before, 1/10 and 4/20 after).
  Also fixed a `FutexManager::wait` lost-wakeup window (enqueue-before-check let a wake be spent on a
  thread that returned EAGAIN; a wake racing a timeout was discarded). A RESIDUAL hang of ~5-17%
  remains on the EAGER path too (all guest threads parked in `FutexManager::wait`) -- a general
  guest lost-wakeup bug independent of fork, tracked in `.gm/prd.yml`
  (`guest-futex-lost-wakeup-residual-hang`), highest priority for app compatibility. Guard-cow itself
  remains the structural limit of user-mode COW. The plan (`.gm/prd.yml`: `native-kernel-cow-fork` and dependents; the shim already
  has a platform-neutral `has_native_fork`/`native_fork` path that Linux and macOS use) replaces it;
  that probe must print `PARENT_DONE torn=0` under the default fork with no flags. Both lazy flags
  stay default OFF. `DE_UP` still not reached; non-lazy is correctness-clean but RAM-crater-limited.
Fully DONE (kept only as a marker so a future pass doesn't re-attempt): the minimal isolated
cross-process AF_UNIX repro; the `Network` shared-arena redesign's `socket_set`/
`LocalPortAllocator`/`closing_in_background`/`queued_for_closure` slice; DISPLAY/`getenv()` as the
`DE_FAILED` cause; AF_UNIX `connect()` `EAGAIN`-vs-`EINPROGRESS`; `pty_registry`/
`daemon_pty_masters` (`syscalls::pty::SharedPtyTable`, live-verified cross-process); fork's
fd-eligibility scan dropping a redirected 0/1/2 (`raw_fd_is_plain_stdio_device`);
`SharedUnixConnectQueue`'s cancel-on-first-non-blocking-miss gap (`UnixStreamState::Connecting`);
both Xvfb SIGSEGVs.

**Open, in rough priority order** (see "Where things stand" at the top of this file for the current,
authoritative detail on item 1 — this list is now just the priority index; do not treat entries here
as more current than that paragraph):

1. **Lazy-fork SIGABRT** (guest pid 51/winpid 2132, `xfce4-session`'s own fork of its next session
   client, `Signal(6)` before `execve`) — both fixed sigreturn-trampoline sub-bugs (104th/105th) are
   long closed; this is a THIRD, distinct, still-open bug, first seen in the real boot shape by the
   113th pass. See "Where things stand" for current status and the 114th pass's comm-inheritance fix
   that reopens the comm-based syscall-timeline filter as the next diagnostic tool for it. **Non-lazy
   config's blocker is purely Track B item 1's RAM crater** (below) — `xfwm4` itself launches and
   survives; no correctness bug remains on that path (109th-113th).
2. `SharedUnixConnectQueue`'s cancel-on-claim-race slot leak — FIXED 62nd. Other AF_UNIX exhaustion
   paths still silent (38th, `unix.rs`): `SharedUnixAddrPresenceTable` capacity-256 overflow; a key
   >108 bytes; backlog ignored on cross-process accept. Abstract sockets CORRECT. (The 103rd pass's
   `ECONNREFUSED`-retry-loop lead against `xfwm4` was a misdiagnosis of a false-alarm WARN, fixed and
   explained by the 109th pass — not a real blocker; don't re-open it without new evidence.)
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
