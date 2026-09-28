# litebox -- current state (2026-09-28, 117th pass)

The authoritative CURRENT-STATE picture: what works, what is broken, what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, or one a later commit superseded, is deleted. Reference detail lives in
`docs/AGENTS_ARCHIVE_*.md` (read for a trail, never as a starting point). This file is also the single
source of truth for standing rules: a future "remember this" is one line plus its pointer here.
Compacted through the 117th pass; the 4th-116th passes' full narrative, including the whole
114th-116th investigation, is in `docs/AGENTS_ARCHIVE_2026-09-28.md` (older:
`_2026-09-22.md`, `_2026-09-23.md`).

## Where things stand (117th pass)

**`DE_UP` REACHED (117th pass, first ever): eager fork + `LANG=C` + event-driven cross-process wake
(`dfdfd26`) -> `_NET_SUPPORTING_WM_CHECK` set 130 s into `.wfgy/pass117_evt_boot2` (window id
0x60008e), 20 processes, ~8 GB private, stable, HOLD loop ran on. Browser access (selkies) and app
acceptance are still untested -- that is the next pickup.** Earlier blockers below are historical. The last five blockers are understood:

- **Eager cross-process fork (`LITEBOX_PROCESS_FORK=1` alone) is correctness-clean end to end**
  (109th-113th: zero `not eligible` fallbacks, `xfce4-session` forks its clients with no crash; the
  Xvfb `Cannot open display` was a harness race, fixed by a real connect probe and `xrdb -nocpp`).
  Boot seed: `.wfgy/pass113_de_only_ready_seed.tar`. Its only blocker is RAM: every run reaches
  `WM_POLL n=3..5` (~115-130s, 15-17 concurrent processes) before free RAM crosses the safety
  kill-switch, just short of `xfwm4` setting the WM check property. Two runs from 6.1-6.4 GB free
  cratered at the same point; it needs more like 8-10 GB free sustained or a real cut in per-process
  cost (Track B item 1). **Per-process cost is not decomposed** (~350 MB-1.1 GB working set each);
  candidates: writable-layer import per child (`litebox_runner_linux_on_windows_userland/src/lib.rs`
  ~2600, copies the whole layer into each child's in-memory fs), rootfs index, guest-memory
  emulation. A 5-process `bash`+`sleep` test is only ~76 MB/process, so the big cost is specific to
  real desktop processes -- measure it (Xvfb under eager fork) before guessing. Measured 117th pass (eager boot from the ready seed,
  `.wfgy/pass117_eager_boot.ps1`, 8 GB free at start): the boot ran 152 s and hit the kill switch at 18
  processes with ~11 GB private commit (~600 MB/process; `procs=5` -> 2 GB, `12` -> 6 GB, `17` ->
  8.8 GB), reaching `WM_POLL n=5` again. `LITEBOX_DIAG_MEM_BREAKDOWN=1` (new, default off,
  `diag_private_memory_breakdown`, `[mem_breakdown]` lines, called in the fork child at each boot
  phase and in the parent at spawn) shows each child holds ~3.4 GB `MEM_MAPPED` read-only (the
  layer tars, shared page cache, NOT commit) plus ~250 MB private, of which ONE 173 MB region at
  guest address `0x7feff05a0000` is present in EVERY child right after `Platform::new()`, even
  `rm`/`mkdir`. The same image with `sleep 1 & wait` as the script (with or without the seed and
  `GLIBC_TUNABLES`) gives only 72-76 MB per child, and `debian:stable-slim` 74 MB, so the 173 MB is
  neither the rootfs index (+2 MB) nor the seed.
  **ROOT-CAUSED (117th)**: it is glibc's `/usr/lib/locale/locale-archive` (181,493,744 B = 173 MB,
  webtop image), mapped whole by `setlocale` for ANY non-`C` locale -- even `C.UTF-8`, because glibc
  looks the name up in the archive first. `de_only.sh` exports `LANG=en_US.UTF-8`; bash re-runs
  `setlocale` on that assignment. The shim's file-mapping fast path (`try_cow_mmap_file`,
  `LITEBOX_COW_MMAP`) is deliberately OFF (lossy partial-unmap zero-fill; `--oci-image` layer bytes
  are heap-owned), so the mapping is a `memcpy` into 173 MB of private commit in every process, and
  every fork child eagerly copies it again: ~430 MB/process of the ~600 MB. **Fix for the boot:
  run with `--env LANG=C`** (and `export LANG=C` in `de_only.sh`/`webtop_stack.sh`; seed
  `.wfgy/pass117_seed.tar`, driver `.wfgy/pass117_eager_boot.ps1`). Verified: root bash private
  506 MB -> 76 MB, region gone. The general defect (any large read-only private file mapping is
  committed per process and per fork) is tracked in `.gm/prd.yml` `file-backed-private-mmap-no-commit`
  and needs lossless partial-unmap recovery plus 64 KiB-aligned or platform-placed views. Full boot
  with `LANG=C` (`.wfgy/pass117_lang_boot1`): **first non-lazy boot ever to run its whole 300 s window
  with no RAM crater** -- 20 processes, ~8 GB private commit, 3.7-4.4 GB free throughout. Every
  desktop client launched (`xfwm4`, `xfsettingsd`, `xfce4-panel`, `xfdesktop`, Thunar, at-spi,
  `ssh-agent`, `gpg-agent`). `DE_UP` still NOT reached: `xfwm4` never set
  `_NET_SUPPORTING_WM_CHECK` in 300 s. Its syscall trace (`LITEBOX_DIAG_SYSCALL_TIMELINE=xfwm4`,
  `.wfgy/pass117_lang_boot2.err.log`, 121k syscalls, 4 threads) shows it ALIVE and busy, not
  hung: synchronous X request/reply loops (`writev`, `ppoll`, `recvmsg`) plus ~2,900 small-file
  opens (themes/pixmaps); it only exited (status 1) when the harness killed Xvfb at 312 s. So the
  open question is SPEED, not a deadlock: ~400 syscalls/s (with tracing) is far below native.
  Next: run untraced for >=15 min (driver's window is now 700 polls, the seed's guest poll loop
  150) with >=6 GB free, then profile where xfwm4's wall time goes (X server under litebox,
  per-syscall cost, per-request round trip). A 15-min run was cut at 130 s by host memory
  pressure (free fell from 4.4 to <0.7 GB with other host apps resident).
- **Lazy fork (`LITEBOX_LAZY_FORK_COMMIT`/`_GUARD_COW`, default OFF) cannot be made correct and is not
  a RAM win: do not patch it further.** A lazy child faults its memory in from the PARENT process; a
  daemonizer (fork, parent exits) leaves the child with zero-filled pages (`lazy_fork_commit.rs`
  unreadable-parent branch). Repro `.wfgy/pass116_orphan.ps1` (lazy dies 5/5, eager correct 3/3);
  this is the real cause of the `ssh-agent` SIGABRT (guest pid 51, comm now visible after the 114th
  comm-inheritance fix) and the dbus SIGSEGVs. Small A/B (`.wfgy/pass116_ramab.ps1`): 381 MB eager vs
  380 MB lazy, no saving. The fix is native kernel-COW fork on Windows through the shim's existing
  platform-neutral `has_native_fork`/`native_fork` path (Linux/macOS already use it) -- PRD row
  `native-kernel-cow-fork`; the spike must be written by a human (my attempt was blocked by policy).
- **Fixed in the 116th pass, all live-verified**: new-thread stack left `PAGE_READONLY` by guard-cow
  (`702c735`, root of the 114th-115th `STATUS_ACCESS_VIOLATION`/bare SIGKILL); `allocate_pages`
  lock-order deadlock (`8b982f3`); `FutexManager::wait` wasted-wake window (`8b982f3`); wait/wake
  Dekker StoreLoad lost wakeup, present on x86 too (`182819d`, probe 50/50 vs 13/20 hung before);
  last-chance minidump filter (`866ebcd`); SeqCst fences after the `WAITING`/`RUNNING_IN_GUEST`
  stores (`7b4cba9`, type-checked only; `ThreadHandle::interrupt`/`LoanList` audited, futex side
  already ordered).
- **Environment limit, 2026-09-28**: this host (15.6 GB) had 0.3-1.7 GB free with Chrome/Discord
  resident; no desktop boot or release build was possible. A boot needs ~5 GB free for its whole
  length; never close the user's applications to get it.

- **Cross-process unix-socket wake is now event-driven (117th, `dfdfd26`)**: it used to be NO wake at
  all -- every blocked read/poll on a shared connection waited out `SHARED_UNIX_POLL_INTERVAL`
  (15 ms), so an X11 round trip cost ~15-30 ms and `xfwm4`'s ~7,000 round trips alone took minutes.
  A send/recv/close on a shared slot now sets the peer host's existing wake event
  (`wake_signal_listener`, handle-cached), the listener (`drain_host`, `xproc.rs`) bumps
  `litebox::event::polling::bump_external_wake_epoch` (a `wait_on_events` waiter treats a changed
  epoch as "re-run `try_op`") and wakes waiting threads (`ThreadHandle::wake_if_waiting`; poll/ppoll
  re-scan on any wake). Measured `.wfgy/pass117_pingpong.sh`-style socketpair ping-pong across a
  cross-process fork: 19.9 ms -> 0.05 ms per round trip. Lesson: a bare `Waker::wake` does NOT
  make a `wait_on_events` waiter retry (its ready-check is the observer flag) -- it needs the epoch.
  Still to do: rendezvous (`accept`/`connect`) and pty waits still poll; cross-process PIPES hang
  when the parent keeps both ends open (PRD `cross-process-pipes-not-shared`); a
  `connect()`/`accept()`-made local pair carried over fork gives EPIPE (PRD
  `shared-unix-bound-socket-fork-broken-pipe`). Full boot with this build: not yet run.

**Next pickup, in order**: (1) with >=8 GB free run `de_only.sh` from the ready seed under eager fork
and reach `DE_UP`; (2) decompose per-process memory (sample `PrivateMemorySize64` per runner against
a `VirtualQuery` breakdown) and cut the biggest piece; (3) app acceptance from a real browser
(terminal, Thunar, Mousepad, settings, panel, Ristretto, a web browser; second client and reconnect)
on Windows, then Linux/macOS builds; (4) native kernel-COW fork, then retire `lazy_fork_commit.rs`.
The PRD (`.gm/prd.yml`, gitignored) carries the full task list.

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

**Pass history (4th-116th)** -- full narrative of every bullet is in the archives; do not re-derive it.

- **4th-74th** (`_2026-09-22.md`/`_2026-09-23.md`): fork fd eligibility, OCI cache, s6-boot, cross-process
  fork's `D==0` design; fixed both Xvfb SIGSEGVs, D-Bus activation's dropped-CLOEXEC-fd bug, per-fork
  rootfs-rebuild RAM cost, the `ssh-agent`/`xfwm4` freeze (`RawMutex::WaiterQueue::with_lock`),
  `SharedUnixConnectQueue::cancel` slot leak; added `litebox_diag::process_timeline`/`socket_read`.
- **75th-82nd**: `xfwm4` launches for the first time (`1d449e6`, writable-layer export path);
  admission control `live_cross_process_fork_children` (cap 6, partial); host-allocator commit
  doubling fixed (`621ee1a`); measured that fork-then-`execve` wastes ~85% of cycle time on eager copy.
- **83rd-105th**: lazy reserve-then-commit fork (`lazy_fork_commit.rs`) and guard-page COW built, five
  bugs fixed, a 7-pass FS_BASE/GS_BASE misattribution resolved (real cause: Bug 4 TOCTOU), the
  sigreturn-trampoline address-collision class fixed twice (`classify_lazy_eligible_groups`,
  `fork_verify.rs`), crater-speed regression fixed (`GUARD_COW_CONCURRENT_CLAIM_CAP = 3`).
- **109th-113th**: `sys_execve` disarm-ordering crash (`24cb72d`); cross-process `kill()`/process
  groups/`SIGCHLD` (`SharedProcessTable`); pty data path unified on `SharedPtyTable`; unix sockets
  (streams, socketpairs, listeners) carried across fork; `Cannot open display` proven a harness race.
- **114th**: cross-process fork children inherit the parent's `comm` (`adopt_forked_process`,
  `LITEBOX_INTERNAL_FORK_CHILD_COMM`); named all litebox-internal threads; `cdb` installed
  (`-xd av -xd sse` as startup flags, not in `-c`); several refuted theories (per-page guard timing,
  `__chkstk`, trampoline collision, writer-thread stack).
- **115th-116th**: see "Where things stand"; the 116th closed the `STATUS_ACCESS_VIOLATION`, two
  hangs and the daemonizer root cause. Standing method that worked: `cdb -pv -p <pid> -c "~*kb 30;
  qd"` stack dumps symbolized with `llvm-symbolizer --obj=<exe> --relative-address`, plus a hang
  catcher that records CPU-delta and a 150 s recovery window (`.wfgy/pass116_hangloop.ps1`).
  Standing lesson: tag every new diagnostic with pid/winpid before concluding anything.

**Fully DONE** (marker so a future pass does not re-attempt): the minimal isolated cross-process
AF_UNIX repro; the `Network` shared-arena `socket_set`/`LocalPortAllocator`/`closing_in_background`/
`queued_for_closure` slice; DISPLAY/`getenv()` as the `DE_FAILED` cause; AF_UNIX `connect()`
`EAGAIN`-vs-`EINPROGRESS`; `SharedPtyTable`; fork's fd scan dropping a redirected 0/1/2;
`SharedUnixConnectQueue`'s cancel gap; both Xvfb SIGSEGVs; the trampoline collisions; the thread-stack
and futex/wait bugs above.

**Open, in rough priority order**:

1. **RAM per guest process** (Track B item 1): needed to reach `DE_UP` on a normal host; see above.
2. **Native kernel-COW fork** on Windows (`.gm/prd.yml` `native-kernel-cow-fork` and dependents).
3. **Writable layer shared across processes** instead of copied into each child (PRD
   `shared-writable-layer-across-processes`); large-content visibility (`/tmp/de.log`) needs a chunked
   publish design, not a wider `SharedFilePublishTable` cap.
4. AF_UNIX exhaustion still silent (`unix.rs`): `SharedUnixAddrPresenceTable` capacity 256, keys
   >108 bytes, backlog ignored on cross-process accept; SCM_RIGHTS across processes. Abstract sockets correct.
5. `SafeZoneAllocator`'s `SpinMutex` has no dead-holder recovery (theoretical); `litebox/src/event/
   wait.rs`'s `unreachable!()` on garbage thread state is NOT debugger-confirmed, don't patch blind.
6. `flock_registry`/`drm`/`evdev` still per-process (`SharedPtyTable` is the template); `timerfd`/
   `signalfd` are the next carriable fd kinds.
7. Selkies: one client per instance, no slot reclaim on reload; macOS guest execution
   (`docs/macos.md`) and Linux/macOS build verification cannot run on this Windows host.

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

- **Archives** (newest first, all under `docs/`) — `AGENTS_ARCHIVE_2026-09-28.md` (pre-117th header + full
  4th-116th pass history: the 114th-116th `STATUS_ACCESS_VIOLATION`/futex/daemonizer investigation),
  `_2026-09-23.md` (70th-113th: `xfwm4` launch, RAM-crater process tree, lazy-fork bug-by-bug, the
  FS_BASE/GS_BASE chase), `_2026-09-22.md` (26th-69th), `_2026-09-18.md` (12th-34th), `_2026-09-17.md`,
  `_2026-09-16.md`, `_2026-09-15.md`, `_2026-09-10.md` (fork fd eligibility, OCI cache, s6-boot).
  Older: `_2026-09-03/05.md`.
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
