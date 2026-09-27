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

**Where things stand, in one paragraph (updated 113th pass)**: three real gaps that used to force
every cross-process fork onto the crash-prone thread-based relocating path are now closed --
109th-112th passes landed cross-process `kill()`/process-groups/`SIGCHLD` siginfo, a single shared
data path for ptys (Ctrl-C, `script`, interactive terminals all verified), and unix-socket carrying
(connected streams, socketpairs, listeners) -- real boots now show **zero** `not eligible` fallbacks
(`.wfgy/pass112_boot{1,2,3}.err.log`). The 103rd pass's `xfce4-session: Cannot open display: .` +
immediate `exit(1)` **is a RACE, not a deterministic bug**: a 113th-pass diagnostic
(`[diag-xfce4session-envp]`, `litebox_shim_linux/src/syscalls/process.rs`) proves `envp` reaches
`execve` with `DISPLAY=:1` intact every time, and a clean run (`.wfgy/pass113_xfce_trace6.err.log`)
shows `xfce4-session`'s own X11 `connect()` succeeding on its normal second attempt (abstract-then-
path fallback) with zero crash -- that run reached `WM_POLL n=4` (~40s of polling) before the
pre-83rd-pass RAM crater (Track B item 1, still open) finally killed it. **Non-lazy
(`LITEBOX_PROCESS_FORK=1` alone) is the furthest any pass has gotten toward `DE_UP` without a crash**
-- the sole remaining blocker on this path is that same RAM crater, not a correctness bug.
**`LITEBOX_LAZY_FORK_COMMIT=1 LITEBOX_LAZY_FORK_GUARD_COW=1`** (the mechanism that actually fixes the
RAM crater) has its own three known sigreturn-trampoline/`fork_verify` bugs fixed (104th/105th) but
was NOT re-verified on a real full boot this pass -- host RAM was too contended (an unrelated,
large Chrome process) to get a clean run; the cheap isolated repro
(`.wfgy/pass105_xset_repro.sh`) should still be re-checked clean before trusting it. **Both lazy-fork
flags remain default OFF. `DE_UP` has not been reached by any of the 113 passes to date, but for the
first time the non-lazy path's own remaining blocker is narrowed to exactly one thing: Track B item
1's RAM crater.** Also 113th: found and fixed the actual mechanism behind the `Cannot open display`
race in the TEST HARNESS itself (not litebox source) -- `de_only.sh`'s own `XSOCK_WAIT_DONE` loop
only checks that Xvfb's socket FILE exists, not that its connection-accepting state is actually
ready, and `PROBE_XSET` ran the real connectivity probe exactly ONCE with no retry before launching
`xfce4-session`, whose own single-shot `XOpenDisplay` has no retry of its own. Fixed harness-side
(new seed `.wfgy/pass113_de_only_ready_seed.tar`, built from `pass103_de_only_trimmed_seed.tar` with
`PROBE_XSET` now retrying its OWN connect probe up to 15s before proceeding) -- live-verified the
retry loop is a correct no-op on the fast path (`attempts=0` in a clean run,
`.wfgy/pass113_ready_boot1.out.log`) but not yet verified to actually CLOSE the race (every attempt
this pass hit the same external RAM contention below before reaching `xfce4-session` again). **Next
pickup**: use `pass113_de_only_ready_seed.tar` (not the older `_trimmed_` one) for all future
non-lazy boot attempts, and once host RAM is genuinely free (6GB+, sustained, no large unrelated
process) run it to completion to confirm `Cannot open display` no longer recurs; separately, re-run
the lazy-mode full boot (`.wfgy/pass110_lazy_boot1.ps1`, also worth pointing at the new ready-seed)
to check whether IT now reaches `DE_UP` (the RAM crater fix + this race fix combined may be enough).
**Second harness fix, same pass**: `de_only.sh`'s own `xrdb "$HOME/.Xresources"` (a one-line file,
`Xcursor.theme: breeze_cursors`, with zero C-preprocessor directives) still ran through xrdb's
default cpp-preprocessing pass, forking a real `sh -> cpp -> cc1` chain for nothing to preprocess --
directly observed causing the ENOMEM that killed a lazy-mode run at exactly this point
(`.wfgy/pass113_lazy_ready1.err.log`: `load_program failed ... path=.../cc1 error=ENOMEM`). Changed
to `xrdb -nocpp` in the same seed. **Live-verified real improvement, both fork paths, several
runs after this fix landed**: zero `Cannot open display`, zero `cc1`/ENOMEM crashes, zero fatal
signals -- every remaining crater is now purely external Windows host RAM exhaustion (confirmed via
`Get-CimInstance Win32_OperatingSystem`/top-process checks showing an unrelated multi-GB browser
process, not litebox's own commit growth exceeding what's actually free) rather than anything
litebox- or harness-side. Non-lazy reached `WM_POLL n=3` at 115s total elapsed in one such run
(`.wfgy/pass113_nocpp_boot1`); lazy-mode reached `DE_LAUNCHED_DIRECT` cleanly in another
(`.wfgy/pass113_lazy_nocpp1`) before an external RAM drop (5.1GB -> 1.1GB within 16s, unrelated to
litebox) cut it short. **With both harness fixes applied, no pass has observed ANY correctness bug
on either fork path anymore -- the only remaining blocker to `DE_UP`, on both paths, is getting one
uninterrupted run under genuinely free, sustained host RAM.** Next pickup: same as above, but use
`pass113_de_only_ready_seed.tar` as it now stands (readiness retry + `xrdb -nocpp`, both already
baked in) -- do not re-derive a fresh seed without both fixes.

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
- **103rd-105th (compacted; full narrative: `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "103rd-105th pass
  full narrative" section)** — 103rd: first pass to directly investigate `DE_FAILED` itself.
  Root-caused it on BOTH configs: lazy-fork config's `xfce4-session` dies of a silent host-level
  `STATUS_ACCESS_VIOLATION` before ever launching `xfwm4` (2/2 "stable" 102nd-pass runs); found a
  much cheaper repro for the same crash class unrelated to `xfce4-session` (`.wfgy/pass103_xset_repro.sh`,
  just `Xvfb`+`xset q`) showing 17/25 faults at identical address `0x7feffffef000`; non-lazy config
  re-verified to get genuinely further (`xfwm4` launches+survives) but still hits the pre-83rd-pass
  RAM crater; fixed a stale-seed-tar repo-hygiene bug. 104th: confirmed+FIXED one real cause of
  `0x7feffffef000` — the sigreturn-trampoline page (deliberately non-exec, meant to always fault) could
  be merged into a lazy-eligible group since it carries no `VM_EXEC`, so `lazy_commit_veh` intercepted
  and infinite-refaulted its deliberate trap; fixed by excluding the trampoline's group in
  `classify_lazy_eligible_groups` plus a defense-in-depth execute-fault decline in `lazy_commit_veh`.
  Live-verified: zero `0x7feffffef000` faults post-fix — but the SAME repro still showed 48 `fatal
  signal Signal(11)` events/`xset rc=139`, now traced to `fork_verify.rs`'s OWN stale-CODE-pointer
  healer hitting the SAME trampoline address via a separate mechanism. 105th: root-caused+FIXED that
  SECOND, independent instance — `run_thread_with_fork_verification` (pass 142/143) arms
  `fork_verify::begin` for every cross-process fork child with an IDENTITY `AddressRelocations`, so
  its own `is_in_source(rip)`/healing logic (zero awareness of "sigreturn" anywhere in its ~2850
  lines) "healed" the same deliberate trampoline fault via a fully independent code path. Fixed by
  threading `Task::sigreturn_trampoline_addr()` through `begin_fork_child_verification` into
  `fork_verify.rs`, which now declines to heal `rip == sigreturn_trampoline` at both code-pointer-heal
  call sites. Live-verified real A/B: 20 occurrences → 0 post-fix; `LITEBOX_PROCESS_FORK=1` alone
  unaffected (0 fatal signals before and after, confirming the new plumbing is a no-op on the
  thread-based path). **Still NOT closed**: the same repro, both fixes applied, still shows 6 fatal
  `Signal(11)`/`xset rc=139` — a THIRD, distinct bug (zero `0x7feffffef000` hits, so not a third
  trampoline instance). New evidence: the clearest crash follows `DIAG_TIMELINE execve
  argv0=/usr/bin/rm` immediately after a `[diag-recover-fsbase]` line. Root cause not found; both
  lazy flags stay default OFF, not safe for a real boot.
- **106th-107th (compacted; full narrative, including the deterministic-address evidence, the
  refuted CoW-mmap hypothesis, and the `allocate_pages`/`reserve_and_commit` near-miss analysis:
  `docs/AGENTS_ARCHIVE_2026-09-23.md`'s "106th-107th pass full narrative" section)** -- both passes
  investigated the 105th pass's open `/usr/bin/rm` crash (a fully deterministic guest `#PF` at
  `cr2=0x111156f60`, litebox's own `Vmem` believing the page present while the real Windows backing
  was not) purely by code reading and re-mining existing logs, ruling out several real candidates
  (`allocate_pages` itself, the CoW-mmap fast path, a stale VMA-adoption diagnostic filter) with
  direct evidence but not finding the actual mechanism. **Superseded**: the 109th pass root-caused
  and fixed this exact crash from a completely different angle (`sys_execve`'s disarm-ordering, not
  a memory-allocation bug at all) -- see that entry below for the real fix. Neither pass's own
  "next pickup" (a live `cdb` attach on `allocate_pages`/`release_memory`) was ever needed.
- **109th -- fixed the crash the 102nd/106th/107th passes chased**: `sys_execve` called
  `end_fork_child_verification()` (which disarms `lazy_fork_commit`'s child-side servicing) at
  function ENTRY, before `copy_vector` read `argv`/`envp` from the old program's still-lazy pages
  and before `kill_other_threads()`/`ElfLoader::new` could still fail and return to the OLD program.
  A failed `copy_vector` (`EFAULT` off a not-yet-serviced lazy page) after disarming left the STILL-
  RUNNING old program's own subsequent heap access unserviceable -- the deterministic
  `cr2=0x111156f60` fault was a `bash` heap address, not a `/usr/bin/rm` mapping at all. Fixed by
  moving the disarm call to directly after `kill_other_threads()` succeeds (litebox-main's own
  `24cb72d`). Also fixed the same pass: the AF_UNIX stream `lookup()`'s presence-miss WARN firing on
  every connect that then succeeds via `connect_cross_process` (`d16e5ce` -- this had been
  misdiagnosed by the 103rd pass as `xfwm4` stuck retrying).
- **110th -- cross-process signal delivery (`LITEBOX_PROCESS_FORK=1`) implemented and verified;
  pty `ISIG` (Ctrl-C/\/Z) and pty-slave carrying across a cross-process fork added.**
  - **Guest pid bug found and fixed**: the parent's `fork()` returned `child_tid` (e.g. `2`) while
    the child called itself by its Windows PID (`std::process::id()`, `ppid` = itself). The child
    now gets `pid:ppid:pgid` via `LITEBOX_INTERNAL_FORK_CHILD_GUEST_IDENTITY`
    (`litebox::platform::ForkChildIdentity`, new `spawn_cross_process_fork_child` parameter), so
    `$BASHPID` in the child == `$!` in the parent. Forked children (both paths) now inherit the
    parent's pgid instead of leading their own group.
  - **Registry**: `SharedProcessTable` (512 pointer-free slots in `GlobalState`) + per-host-process
    `GlobalStateHandle::xproc_local` (pid -> `Weak<Process>`). Registered: bootstrap, every
    thread-based fork child, every cross-process child (pre-registered by the parent before the
    spawn with the parent's host pid, repointed to the child's host pid after it, re-found by the
    child's own `adopt_forked_process`). Released at `wait4` reap; a slot whose own host process is
    dead is released on the next send to it (orphans) or when the table is full. `getpgid`/
    `setpgid`/`setsid` go through it, so `kill(-pgid)`/`kill(0)`/`kill(-1)` reach every host
    process. New platform hooks (`litebox/src/platform/mod.rs`): `current_host_pid` (0 = registry
    off, the default for non-Windows platforms), `cross_process_child_host_pid`,
    `start_signal_wake_listener`, `wake_signal_listener`, `terminate_host_process`.
  - **Verified** (`debian:stable-slim`, release, `LITEBOX_PROCESS_FORK=1`, logs `.wfgy/pass110_*.log`):
    `sleep 30 & kill -0/-TERM; wait` → `kill0_rc=0 term_rc=0 wait_status=143` (was ESRCH + a full
    30s wait with status 0); subshell `trap ... TERM` → `got_term`, status 3; `kill -9` → 137;
    `set -m` group kill of a job and of a job whose own children inherited its pgid → all members
    143, a job in another group untouched; a grandchild (not the caller's child) found via `$( )`
    killed by pid, then `kill -0` → ESRCH; `for /bin/true` loop → `ok`. Thread path (flag unset):
    identical to the pre-change binary (test 1 passes; subshell/`sleep` children still `Aborted`
    134 with `A NULL argv[0] was passed through an exec system call` on BOTH binaries — the
    thread-based fork's own pre-existing corruption, not this change).
  - **pty**: `PtyEnd::write` on a master with `ISIG` consumes `VINTR`/`VQUIT`/`VSUSP` and signals
    the pty's foreground group (`xproc_signal_group`); a fresh pty now reports Linux's default
    `ISIG` + `c_cc`. `PtyStateRef::Local` getters now prefer the published shared slot (another
    process's `TIOCSCTTY`/`TCSETS` was invisible to the allocating process). A pty SLAVE fd is now
    carried across a cross-process fork by reopening `/dev/pts/<id>`
    (`carriable_pty_slave_for_raw_fd`) instead of being dropped — the child of `forkpty()`/`script`
    and every command a shell in a pty runs used to lose its stdio. Verified with a `perl` forkpty
    harness: `^C` written to the master → child `bash` trap exits 5 / plain `sleep` dies
    `WIFSIGNALED(2)`.
  - **Open**: (pty output gap: fixed 111th, below.) No SIGSTOP/SIGCONT job-control semantics across hosts; no
    `si_pid` in the cross-process `siginfo`; SIGKILL-by-`TerminateProcess` also kills any
    thread-based descendants living in that host process; no input-queue flush/`^C` echo on
    `ISIG`; tkill/tgkill to a thread in another host process still `ESRCH`.

- **111th -- pty data path unified on `SharedPtyTable`; `SIGCHLD` carries a real child siginfo.**
  - **Pty**: a published pty (any free slot of the 8) has NO in-process channel any more: `/dev/ptmx`
    returns a `SharedMaster`, every `/dev/pts/<id>`/`TIOCGPTPEER` open a new `SharedSlave`, and all
    reads/writes/readiness go through the slot's rings (before, a local master read only its
    channel while other processes' slave output went into a ring nobody read). Line discipline on
    the ring path: `ISIG`, `ICRNL` (Enter from a terminal emulator is ``), `ECHO`, `OPOST|ONLCR`,
    DSR reply. Open slaves are counted per host process in the slot; once one was opened and none
    remains (or its host died) master `read` = `EIO` and poll = `IN|HUP`; master closed/host gone ->
    slave read EOF, slave write `EIO`. `poll`/`epoll` put `Shared*` pty fds on the existing 15ms
    bounded-repoll path (no cross-process wake exists). Fresh ptys now default to Linux's
    `ICRNL`+`ECHO` (+`ISIG`, `ONLCR`, `CS8|CREAD|B38400`); without `ECHO` readline never shows typed
    input. `ICANON` stays unset (no canonical buffering). `--pty-mode` keeps no-`ECHO`/no-`ICRNL`.
    Only a 9th simultaneous pty falls back to the old in-process pair (process-local).
  - **`SIGCHLD`**: both the thread-path exit notify and the cross-process exit notifier sent it with
    `SI_USER`; util-linux `script` reaps only on `CLD_EXITED`/`CLD_KILLED` and hung forever.
    `siginfo_child` now sets code/pid/uid/status (`spawn_cross_process_exit_notifier`'s callback
    receives the raw exit code), and signalfd fills `ssi_pid`/`ssi_uid`/`ssi_status`.
  - **Verified** (`debian:stable-slim`, logs `.wfgy/pass111_*.log`): `script -qc 'echo
    hello_from_child; ls /' /dev/null` prints both and exits 0 in ~1s (was: no output, hang);
    forkpty harness `.wfgy/pass111_forkpty.pl` reads the child's `child_says_hi
...` then
    `EIO`(5); interactive `bash -i` on the slave: typed `echo x` echoed, `` -> runs, `x` read
    back, `exit` -> `EIO`; `^C` -> trap exit 5; single-process pty (`.wfgy/pass111_nofork.pl`, with and
    without `LITEBOX_PROCESS_FORK`) and 9-pty fallback and `--pty-mode` still work; pass-110 signal
    tests unchanged.
  - **Open**: no canonical-mode line editing (erase/kill/`^D`-as-EOF); `ECHOCTL` not rendered
    (`^C` not echoed, a `^D` is echoed raw); ring is 2 KiB per direction; master/slave readiness
    across processes is polled at 15ms, not event-driven.

- **112th -- unix sockets carried across a cross-process fork (no more thread-path fallback).**
  `UnixSocket::fork_carry`/`from_fork_spec` (`unix.rs`) + `ForkInheritedShimFd` (opaque spec in
  `LITEBOX_INTERNAL_FORK_CHILD_SHIM_FDS`, rebuilt by `install_shim_fd_at_fd`). Connected
  stream/seqpacket: a local pair is PROMOTED onto a `SharedUnixConnTable` slot (`ConnLink`
  shared by both ends; after promotion both local ends and the child use only the slot, each
  local end first draining its own pre-promotion channel; the carried end's unread queue moves
  into the slot; SEQPACKET slots are record-framed). Slot holders are counted per side per HOST
  process (`SharedConnSlot::hold`), parent pre-holds for the child, child transfers the count;
  peer gets EOF/EPIPE when a side has no live holder. Listener: rebuilt in the child, advertised in
  `unix_addr_presence` under the child pid at fork time; child `accept()` takes cross-process
  connects from the shared queue (connects from the parent's own host still go to the parent's
  backlog). Fresh unbound stream/dgram sockets recreated. CLOEXEC rule unchanged except addressless
  socketpairs are carried. Fixed along the way: dead-slot reclaim compared GUEST pids to host
  processes since the 110th pass (could free live slots) -- now holder-based; `fs::import::
  import_all` aborted at the first failing entry and every child re-exports Xvfb's 0444
  `/tmp/.X1-lock`, so every cross-process child's writes were silently dropped (89-90 `failed to
  import` WARNs/boot) -- now chmod-write-restore + keep going, path in the error.
  - **Verified** (`.wfgy/pass112_unix.pl`, `pass112_*.log`): socketpair round trip + pre-fork queued
    data + EOF on child exit; SEQPACKET `m1`/`m2` boundaries; grandchild uses inherited connected
    socket to a server in a third process which `accept()`s on an inherited listener; bash holding
    a unix socket runs the trap/kill test cross-process (`got_term`, 3, no fallback -- same test
    aborts with `invalid stdio handle` on the thread path); 110th/111th tests unchanged.
  - **Boots** (`pass103_trimmed_boot1.ps1`, non-lazy): 0 `not eligible` in all 3 (baseline 11).
    boot1: xfwm4/xfsettingsd/xfce4-panel launched, WM_POLL n=5, RAM crater kill at 140s. boot2/3:
    no crater (>=3.5GB free for 300s) but `xfce4-session` never spawned its children, one
    `connect_cross_process ... never accepted` to `/tmp/.X11-unix/X1`, `DE_FAILED after 200s`;
    boot3 showed the import bug above (fixed after; not re-booted -- host RAM fell to 3.8GB).
    `DE_UP` not reached.
  - **Open**: SCM_RIGHTS over a slot = `EOPNOTSUPP`+warn (dbus fd passing across processes);
    datagram socketpairs and bound/connected dgram, bound-unconnected and connecting streams still
    refuse; slot ring is 2 KiB/direction, 64 slots, 15ms repoll; a cross-process `connect()`ed
    SEQPACKET is still unframed; child-listener sees only cross-process connects.
- **113th -- fixed a real diagnostic-tooling bug that was hiding useful evidence, confirmed
  `xfce4-session`'s "Cannot open display: ." is a RACE not a deterministic bug, and got the
  furthest yet toward `DE_UP` on the non-lazy path (no crash, RAM crater is the sole blocker).**
  - **`is_syscall_timeline_target_comm` (`litebox_shim_linux/src/diag.rs`) fixed**:
    `target.starts_with(trimmed)` is trivially true when `trimmed` (`comm`) is empty, which it is
    for every guest thread between `clone()` and its first `execve()` -- so
    `LITEBOX_DIAG_SYSCALL_TIMELINE=xfce4-session` traced the pre-exec bootstrap syscalls of EVERY
    forked thread on the whole boot, never reaching `xfce4-session` at all before the volume
    exhausted host RAM. Now requires a non-empty `comm`. This tool (the project's own "what is this
    specific process doing" instrument) is now trustworthy again for a single-comm target.
  - **New permanent diagnostic**: `[diag-xfce4session-envp]` in `sys_execve`
    (`litebox_shim_linux/src/syscalls/process.rs`), fires only when the execve path ends in
    `xfce4-session`, dumps `fork_relocations.is_some()` and the `DISPLAY` envp entry straight to
    stderr. Live-confirmed (`.wfgy/pass113_envp_diag2.err.log`): `DISPLAY=:1` reaches `execve`
    intact every time -- rules OUT the `copy_vector`/`heal()` staleness class (that function's own
    doc comment already documents a similar-LOOKING but mechanistically DIFFERENT prior DISPLAY
    bug, on the old thread-based eager-duplicate path) as the cause of the 103rd/112th passes'
    symptom.
  - **Using the now-fixed syscall timeline** (`.wfgy/pass113_xfce_trace6.err.log`, a clean, non-
    crashing run): `xfce4-session`'s own `socket()`+`connect()` to the X11 socket fails on its
    FIRST attempt (`addrlen=25`, almost certainly the abstract-namespace form) then SUCCEEDS on a
    second `socket()`+`connect()` (`addrlen=20`, the path form) -- this is normal, expected libxcb
    client behavior, not a bug. This run printed no `Cannot open display` at all, reached
    `DE_LAUNCHED_DIRECT` and `WM_POLL n=1`..`n=4` (~40s of polling, zero crash), and was only ever
    stopped by an external cutoff -- the SAME `pass103_trimmed_boot1.ps1` script that produced
    `Cannot open display` + immediate `exit(1)` in the 112th pass's own `boot2`/`boot3` runs, on the
    same code. Conclusion: that symptom is a **timing race** (most likely `xfce4-session`'s first
    connection attempt landing before Xvfb's socket is fully ready under some schedules, or a
    fork-admission-timing interaction), not a deterministic corruption bug -- do not re-open the
    `copy_vector`/envp angle without NEW evidence, and don't assume a future "Cannot open display"
    sighting is this exact mechanism without checking whether it's a repeat of THIS race first.
  - **RAM crater re-confirmed as the real, sole remaining blocker on the non-lazy path**: the clean
    run above (`.wfgy/pass113_full_boot.poll.log`) ran 300s' worth of the script's own loop,
    reaching `WM_POLL n=4`/128s elapsed before falling below the kill-switch threshold at 14-15
    concurrent processes -- Track B item 1's own long-documented shape, not a new bug.
  - **Lazy-mode NOT re-verified on a real boot this pass**: host RAM was severely, externally
    contended for most of this pass (an unrelated ~7GB Chrome process; free RAM oscillated between
    ~0.1GB and ~6.5GB on a ~1-2 minute cycle, unrelated to anything litebox did) -- two lazy-mode
    boot attempts both hit genuine RAM exhaustion (`ENOMEM` loading `cc1`, i.e. real memory
    pressure, not a new correctness bug) before reaching `xfce4-session` at all. **Next pickup**:
    once host RAM is genuinely free and STAYS free (6GB+ sustained, no large unrelated process),
    re-run `.wfgy/pass110_lazy_boot1.ps1` (or a fresh equivalent) to completion and check whether it
    now reaches `DE_UP` -- every correctness bug found so far this session (sigreturn-trampoline
    x2, unix-socket carrying, signals, ptys) applies equally to both fork paths, so there is no
    known reason left to expect lazy-mode NOT to reach `DE_UP` if it gets enough real RAM headroom
    to avoid an unrelated crash first.

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
     LITEBOX_LAZY_FORK_COMMIT=1 LITEBOX_LAZY_FORK_GUARD_COW=1`. **106th pass: the `[diag-recover-
     fsbase]` co-location is a red herring** (its own printed `fsbase=` value is confirmed healthy
     every time -- ruled out by direct evidence, not guessing) **and the crash is NOT a re-instance of
     either fixed trampoline bug** (zero `0x7feffffef000` hits). New hard evidence via
     `litebox_shim_linux::lib.rs`'s existing `diag-guest-exception` diagnostic
     (`litebox_shim_linux=debug`): a fully deterministic guest user-mode `#PF`
     (`cr2=0x111156f60 rip=0x7feffc2f3e56`, byte-identical across independent pids/runs -- not a
     race) where litebox's own guest `Vmem` tracking believes the address is validly mapped
     (`range_start=0x111148000 range_end=0x111169000 flags=VM_READ|VM_WRITE|...`) but the real
     Windows memory is not-present -- a genuine present-vs-tracked desync in a mapping
     `/usr/bin/rm`'s own post-`execve` load created. **Also ruled out this pass**: `allocate_pages`
     itself as the creation site for this mapping -- `LITEBOX_DIAG_MM=1`'s own `diag-commit`/`DIAG
     allocate_pages` lines are confirmed reaching this exact process (its later teardown
     `diag-decommit` lines fire) yet none fire for it before the crash, and a full read of
     `allocate_pages`'s entire fixed-address commit path (`lib.rs:7785-8541`) found no
     silently-discarded commit failure anywhere in it. Next pickup, precise: read litebox's
     memory-mapped/CoW-view file-backed ELF-segment loading path (not `allocate_pages`) for how it
     can register a guest `Vmem` entry as present without the real Windows backing being committed;
     if code reading doesn't resolve it, a live `cdb -p` attach (debug build; invasive `-p`, not
     `-pv`, per the 93rd pass's own correction) breaking on `/usr/bin/rm`'s own post-`execve` resume
     is the fallback. Do NOT retune `live_cross_process_fork_children`'s admission cap or
     `GUARD_COW_CONCURRENT_CLAIM_CAP` in response -- this is a correctness bug neither capacity lever
     can fix or should mask.
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
