# litebox -- current state (2026-10-03, compacted; 121st pass)

CURRENT-STATE: what works, what is broken, what to do next. Every claim carries a commit sha or `file:line`;
a claim nobody can point at, or one a later commit superseded, is deleted. Detail (4th-120th passes) is in
`docs/AGENTS_ARCHIVE_*.md` -- a trail, never a starting point. Single source of truth for standing rules.

## Where things stand

**A real XFCE desktop runs and is usable**: `Xvfb` + `xfce4-session` + `selkies`, host proxy
`--publish 8081:8081`, `DE_UP` in 25-40s. Verified via real Chrome CDP (2026-09-30, `.wfgy/pass118_fin11.*`):
Applications menu, xfce4-terminal (prompt, `tty`, job control, pipelines, `su`, DNS), Mousepad, Settings,
Thunar, `ls /proc/self/fd`, `apt-get update`+`install`; 12+ min soak, 30s probes 200, 24 host processes,
3.3GB WS. Typing drops chars when the host is starved; chrome-devtools `click` cannot hit injected overlays
-- use `Alt+Tab`/keyboard.

Fixed (newest first; mechanism in the archive unless noted):
- `a1423ee` a carried "child writes" pipe is a local pipe + a pump, so `write(2)` returns before the bytes
  reach the parent -- a bulk producer that exits at once lost its tail (`dd bs=1024 count=200 | wc -c` =
  151552 of 204800). The child's exit now waits for its write pumps to drain (bounded 5s).
- `30d8f43` a `Source` pipe bridge waited for `owners()==1`, but HOLDING an end is not READING it: a wrapper
  (`timeout`/`env`/`nohup`/`setpriv`) keeps stdin open for the child it forks -- deadlock (`printf x |
  timeout 12 sh -s` rc=124 25/25). `ForkPipeBridge::pending_bytes` (FIONREAD) breaks the wait when a
  non-zero count sits unchanged 500ms.
- `7d2a6a7` `SHARED_UNIX_CONN_CAPACITY` 1024->4096, and every unadopted-carry discard path releases the
  sender's slot hold: **chromium renders a page with its OWN sandbox active**. `ad2659f` `SECCOMP_RET_TRAP`
  returns the syscall number; a connected endpoint's `Conn` hold ships with the carry spec.
- `067367b` fork children keep committed fork padding (`VM_OWN_FORK_PADDING`); cross-process `/proc/<pid>`;
  no-restorer frames refused; AV heals bounded (`MAX_AV_PATH_HEALS`).
- `a2f3eb9`+`7d2a6a7` unix conn capacity 64->4096 (`unix.rs`, arena-backed). `9412184` writable-layer-only
  file carries by content (`T|`). `6beb669` SCM_RIGHTS + fork share one unix carry path.
- `19eab93` PID namespaces; crashpad (`yama/ptrace_scope`, `PR_SET_PTRACER`, SCM_CREDENTIALS);
  `CLONE_FS`/`CLONE_FILES` clones stay same-process. `8478b15` fork children keep `PROT_NONE` reservations.
- `3bfe283` `CLONE_NEWUSER` credentials cross fork; `VM_MAY_ACCESS_FLAGS`. `883cab4` user namespaces (id
  maps, `setgroups` first) + `Backend::services_own_writes`. `e608959` real `chroot(2)`. `2bb71cb` real
  `seccomp(2)` (classic-BPF on the raw nr at the top of `Task::do_syscall`).
- `668d084` `idle_trim.rs` (`LITEBOX_IDLE_TRIM=0` off). `f271ed2` flat rootfs index. `d428acd`
  `with_root_identity` takes the per-thread root guard too. `a2ebff6` CLOEXEC survives a pipe-end fork
  carry. `748c4e5` registry + record locks out of the shared arena.
- `9014df7` unix `EPOLLOUT` only when the blocked message fits -- **reports "chromium headless prints
  example.com"**, not reproduced since. `582cc4b` chromium profile dirs use the shared spill. `f36621a` one
  `VirtualQuery` per region: fork 3-5s -> ~0.5s.
- Older: `a42d9d0` pipeline EOF; `d944c66` net_lock not orphaned on exit; `f594693` mutable `Credentials` +
  `IP_RECVERR`; `2b7d7df` TCP `mark_peer_closed()`; `88f632f` device write; `f0ecbb0` `getsockname`;
  `290d4d4` `mprotect` rounding; `3ee1ce7`/`bff1d0b`/`a61ed74` lock dead-holder recovery; `0cda0ec` lazy
  range + `epoll` re-poll; `d383f90`/`d1dae93` pty; `820c2d6` watchdog arming; `bb518ca` per-fork env vars
  consumed by `take_fork_env` -- **any new per-child env var needs a remove after adoption**.
- Memory (`b11f674`, `6fd27b7`, `d07a7f9`, `f271ed2`, `668d084`): fork child 41MB->8MB resident; stack
  private resident 2246MB->1222MB; idle trim WSsum 2.8GB->~0.4GB. Beware root-vs-child heap layout
  differences exposing shared-struct host pointers (`bootstrap_process`).

## Open, in rough priority order

1. **The whole-guest freeze: threads across many host processes parked in `sys_close`** (chrD3/chrD4/chrD5;
   live `cdb` + `[diag-lockstall]`). Not yet root-caused -- "Frozen-in-`close(2)`" below IS the live
   investigation.
1b. **Host memory is the binding constraint, not a litebox bug.** Left: selkies/Xvfb heaps (150-250MB private
   RW, legitimate), guest `PROT_NONE` reservations are committed, an idle process burns 1-3% CPU. Diagnose
   with `LITEBOX_DIAG_ALLOC_STACK=1` (symbolize with `llvm-symbolizer`) and `LITEBOX_DIAG_MEM_BREAKDOWN=1`;
   sample with `.wfgy/memsamp.ps1`, `.wfgy/wsmap.ps1 -ProcId`.
1c. Chrome (the user's own, ~6GB) leaves 0.3-2GB free, so runs die on the driver's `KILL low memory` guard
   (`avail<120`); check `Get-Counter '\Memory\Available MBytes'` first (gate 2500MB in `chrdesk_lock.ps1`,
   1000MB in `pass118_full_err.ps1`).
2. Verify in the full stack (committed, not yet seen in a browser): `3e1ef47` SCM_RIGHTS over a
   cross-process unix connection (thunar's D-Bus call passes a dup of stdin); `b012910` lazy file map stale
   entries over an execve'd fork child's libraries; `08ae94f` `/proc/<pid>/fd`. Repro `.wfgy/th1.ps1 -Run
   th1|th2|th3`.
3. Selkies: one client per instance, no slot reclaim on reload.
4. Native kernel-COW fork on Windows (`.gm/prd.yml` `native-kernel-cow-fork`); writable layer shared across
   processes; AF_UNIX exhaustion silent (`SharedUnixAddrPresenceTable` 256 slots, keys >108 bytes);
   `flock_registry`/`drm`/`evdev` per-process; `timerfd`/`signalfd` uncarriable; fixed-address (non-PIE) exec
   from a same-process vfork child collides (gcc). macOS/Linux builds unverifiable here.

## How to run and drive it (harness lessons that cost sessions)

- Cheap repro: `target/release/litebox_runner_linux_on_windows_userland.exe -Z --oci-image docker.io/
  library/debian:stable-slim -- /bin/bash -c '<script>'` (or the cached `webtop:debian-xfce`).
  **PowerShell, never Git Bash** for guest paths (it rewrites `/abs/guest/paths`); single quotes only inside
  `-c`; redirect with `cmd /c "... < script.sh > out 2> err"` (`Start-Process` redirection kills the runner;
  `*>` logs are UTF-16LE). Confirm the exe mtime postdates the newest commit. **Build only `cargo build
  --release -p litebox_runner_linux_on_windows_userland`; the whole workspace does not build on Windows.**
- Full stack: `.wfgy/pass118_full.ps1 -Run <name> -MaxSeconds N` (+`.sh`); waits for 2.5GB free RAM (the log
  appears late) and **terminates every `litebox_runner` at its cap -- a stale older driver kills a newer
  run** (cost two sessions). Kill leftover driver powershells first; give a cap longer than needed. Variants
  `pass118_full_err.ps1` (adds `litebox_diag::stderr_capture=debug`), `pass118_full_a11y.ps1`.
  `acquire_boot_lock()` refuses a second boot while an orphaned run lives.
- Browser: `mcp__chrome-devtools__*` for real CDP input. `click` needs a uid, so inject a fixed
  `pointer-events:none`, opacity .01 `<button id=probe>` at the wanted x,y and click that (the real mouse
  event lands on the video). `Control+Alt+t` opens Terminal (allow 30s); gm `cdp` is JS-eval only.
- **Guest scripts must be LF.** A CRLF `.wfgy/*.sh` turns every `\`-continued line into its own command
  (`/bin/bash: line 55: +iglx: command not found`, boot exits 127) -- looks like a litebox bug and is not
  one. Git's `core.autocrlf` reintroduces it.
- Cheap guest repro, no file in the guest: `.wfgy/guest2.ps1 -Script <sh> -Run <name> -Secs <n>` (base64's
  the script onto the command line; sets LITEBOX_PROCESS_FORK=1, LITEBOX_LAZY_FILE_MAP=1,
  LITEBOX_OCI_USE_LAST_RESOLVED=1; `-ExtraEnv "K=V;K2=V2"`). **It kills every `litebox_runner*` at start, so
  runs must be sequential.**
- Diagnostics: `cdb -pv -p <pid> -xd av -xd sse -c "~*kb 25; qd"` (NOT on PATH: use
  `C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe`; `-pv`/`qd`, never bare `q`), symbolized
  with `llvm-symbolizer --obj=<exe> --relative-address` (`.wfgy/symstk.py`; `symstk2.py` does 30 frames;
  release ICF mislabels callers); `litebox_diag::{process_timeline,socket_read,unix_conn_teardown,
  stderr_capture}=debug`; `LITEBOX_DIAG_NO_EXTERNAL_FAULT_WATCHDOG=1`/`LITEBOX_DIAG_NO_FAULT_WATCHDOG=1`
  before any attach. A cross-process child whose exit code lacks the `0xC0DE` marker decodes as SIGKILL, so
  `rc=137` also means "host process died some other way".
- **Before blaming litebox for a death or stall, correlate with the harness's own time caps, kill loops and
  the host's free RAM first.**
- **The image's `Xvfb` is XLibre now and ABORTS when `/tmp/.X11-unix` already exists with the wrong mode**
  (`BUG: ../os/log.c:620 in vpnprintf()`; every X client then fails `unable to open display :1`). So
  `mkdir -p /tmp/.X11-unix && chmod 1777 /tmp/.X11-unix` BEFORE Xvfb (in
  `.wfgy/{xv1,chrdesk,pass118_full}.sh`). A dead X server looks exactly like a litebox unix socket bug --
  check the server's own output first.
- **Run dbus-daemon non-forking** (`dbus-launch` daemonizing breaks connects); for XFCE use
  `xfce4-session`. `.wfgy/webtop_stack.sh` is embedded in `.wfgy/webtop_seed.tar` (re-tar after edits).
  Readbacks inside a boot script use `$( )`/pipes, not `cmd > /tmp/f` + a sibling's read.
- **Repo hygiene**: layer tars, frame dumps, debug logs never in git (`.wfgy/` is ignored); no test files;
  commit as lanmower only.

## Frozen-in-`close(2)`: the live investigation (chrD3/chrD4/chrD5, 2026-10-03)

A full desktop run wedges right after `DE_UP` (55s): selkies is launched (`SELKIES_LAUNCHED_INSTRUMENTED`,
one `DEBUG:ws:Legacy Mode ENABLED` line) and never prints `Selkies server running`; the guest's own `curl`
probes never print either -- the WHOLE guest freezes, not just selkies.

chrD5's `LITEBOX_DIAG_LOCKSTALL=1` split it in two:
- **The freeze (49 waits)**: 16+ processes parked `val=2` on ONE arena mutex `0x7ff803061028` (>=
  `SHARED_KERNEL_HEAP_BASE` `0x7FF8_0000_0000`, `lib.rs:11370`, so genuinely cross-process), whose
  `holder_pid` is the process hosting guest pid 20 (selkies) and is ALIVE -- 295 "RawMutex held by a live
  process" warnings, ZERO recoveries. A live thread sitting on a shared lock is the freeze;
  `try_recover_from_dead_holder` rightly refuses to steal from a live holder.
- **A separate spin (83 waits)**: `val=1 now=1 holder_pid=0 occupied=1 queued=true pending_wake=false`
  (each its own per-thread waker condvar) where `chunks` climbs to 960 while `wall_ms` stays 14 -- i.e.
  `WaitForSingleObject` returns `WAIT_TIMEOUT` without ever blocking, so the loop busy-spins (236k diag
  lines in one run). Not the freeze, but it burns CPU. `chunk_ms` and `rc` are logged too as of chrD6.

`cdb` of the holder process (`.wfgy/chrD5_cdb_135856.txt`, 10 threads): guest pid 20's threads blocked in
`Pipes::read` -> `RawMutex::block`, `RawRwLock::write_contended` (`Pipes::create_pipe`,
`in_mem::FileSystem::new`), `RawRwLock::read_contended` (`read_dir`, `Network::perform_platform_
interaction`); two `spawn_fork_child_pipe_pump` threads (one in `Pipes::read`, one in `sleep`). Release
ICF mislabels callers -- `sys_close` appears in stacks it is not part of.

Ruled out earlier: the `close(2)` HUP/linger wait (`wait_on_events` runs `try_op` FIRST, `polling.rs:59`;
only `GracefulIfNoPendingData` can park, `net/mod.rs:1471`; `close_socket` now warns when it defers -- 0
hits); the TUN thread; panics/resets; waiter-queue overflow; dropped cross-process wakes.

`holder_tid` (chrD6) names the actual thread: `RawMutex::note_locked` stores `GetCurrentThreadId()`
alongside `holder_pid`, and both long-wait warnings print it -- match it against cdb's `~*kb` TIDs.

Why it was silent before: `WaitState::commit_wait` (`wait.rs:417`) parks with `val=1 holder_pid=0`, the warn
gate is `(holder != 0 || val == 2)`, and dead-holder recovery refuses `holder_pid == 0` ("never guess").

Reading the logs: `chrD*.err.log` is BINARY to gm codesearch (NULs) -- use `.wfgy/logscan.py <file>
<term...>` or inline python; `.wfgy/scan_waiters.py` parses an arena `dd` dump for `WaiterQueue` slots.

## Standing lessons and hard constraints

- **No WSL/hypervisor ever.** Never `bcdedit /debug on` without a kernel debugger. A process that resists
  `Stop-Process` needs WMI `Terminate`.
- **Never run two full-stack verifications concurrently**; watch `Available MBytes`, kill on a falling trend.
- **Guest-reachable code returns an errno, never a panic** (the host process IS the whole guest session).
  Refusal errno is API contract: EPERM lets callers degrade, EINVAL/ENOSYS fails them hard.
- **`LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check.** Never time litebox with one host
  process per datapoint (spawn costs 1.6-2.3s). Never trust a container tag name for its contents.
- **A `TypedFd` index is valid only against the `Descriptors` that inserted it**; shared-memory structs must
  hold no process-relative pointers (`Network`, `Pipes`, `FutexManager` were all this bug). A mutable table
  on the `SharedUnixAddrPresenceTable` pattern needs every write path mirrored to the shared side.
- **Cross-process fork (`LITEBOX_PROCESS_FORK=1`)**: a genuine `D==0` fork (`spawn_cross_process_fork_child`,
  `litebox_platform_windows_userland/src/lib.rs:13609`); pipes/regular files/eventfds/pty/unix sockets
  carried, cloexec+pty fds dropped; `live_cross_process_fork_children` caps concurrent children at 6. A run
  took this path only if `[process_fork_diag] task-resume-probe` lines exist. Cross-process `kill()` goes
  through `GlobalState::process_table` (`syscalls/signal/xproc.rs`). **The env var is PRESENCE-CHECKED**
  (`std::env::var_os`): `LITEBOX_PROCESS_FORK=0` still ENABLES it -- unset it for the same-process path. A
  `socketpair` fd is carried. A child's guest pid is the parent's `child_tid`; its Windows pid is `winpid=`.
- **`chroot(2)` is real (e608959)**: `FsState.root` is shared by `CLONE_FS`, so a chroot on any task sharing
  that state roots all of them, parent included; `cwd` is root-space, dirfd-relative paths stay unrooted.
  Do NOT test `CLONE_FS` with a raw `clone(CLONE_VM|CLONE_VFORK|CLONE_FS)` from CPython: the parent never
  resumes, the process dies 139; use pthreads.
- **Logs**: default `warn,...fork_verify=error`; prefer dedicated low-overhead targets over blanket module
  debug (`syscalls::file=debug` floods 50MB/s). A boot whose log stops is usually a dead root runner.
  **Verbosity comes from `LITEBOX_LOG`, not `RUST_LOG`.** Disk hygiene: sample or cap repeated messages
  (`MAX_AV_PATH_HEALS`, `AV_HEAL_LOG_SAMPLE_STRIDE`) -- one duplicated warn made a 1GB log in 2.5 min, and
  an ungated per-wait warn made 190MB in one run.

## Linux runner (`litebox_runner_linux_userland`, merged from `claude/modest-feynman-3zpzop`)

Cloud sessions are Linux; there `webtop:debian-xfce` (`--initial-files rootfs.tar --rewrite-syscalls --uid 0
--gid 0 --pid1 --tun-device-name tun0 ... /bin/sh /init`) boots: s6, Xvfb, xfwm4/panel/xfdesktop, nginx,
pulseaudio, dbus, Selkies; a host browser at `http://10.0.0.2:3000` (TUN, host 10.0.0.1) drives it. Harness
`tools/webtop/`. Not re-verified after the merge.
- Native fork (`has_native_fork()==true`): shared-arena fork, pool-backed shared memory; hooks `waitpid`/
  `waitid(WNOWAIT)`; `SYS_wait4`/`SYS_waitid` seccomp-allowed (a blocked host syscall answers EINVAL and
  looks like a hang); `exit_native_fork_child` ends the child with the guest status. A native-fork child
  COW-copies every private kernel structure, so shared state must live in the arena. The thread-based fork
  corrupts guest memory on Linux -- not a substitute.
- Per-process `/proc/self` identity, `/proc/<pid>/{task,oom_score_adj,environ,fd}`; real pty line
  discipline; SCM_CREDENTIALS; shared-mapping coherence; per-thread fs "act as root" guard; the network
  worker never blocks on a per-descriptor lock (`iter_nowait`); runner panic = `_exit(134)`; big read-only
  private file maps are ONE shared object; freed heap >=1MiB returns pages (`MADV_REMOVE`). Plateau ~9GB.
- Added: inotify, `MSG_PEEK` on unix+inet (used to consume bytes; broke TLS), xattr stubs, tar mtimes,
  `mincore`, `copy_file_range`, `splice`, `rt_sigtimedwait`, `rt_(tg)sigqueueinfo`, `O_TMPFILE`,
  NETLINK_ROUTE dumps, `pidfd_open`/`waitid(P_PIDFD)` (also reaps cross-process children), `getrusage`/
  `mlock`, ptrace answers EPERM.
- Dead-holder recovery for `RwLock` and platform `RawMutex`: a waiter blocked 2s checks `tkill(tid,0)`; up
  to 4 readers tracked. A panic holding the layered-fs root write lock hangs every later `open`. Debug kit:
  `LITEBOX_DIAG_FAULT=1 LITEBOX_PRINT_EXE_BASE=1` + frame-pointer build; `LITEBOX_DIAG_BIGALLOC=1`; stalled
  boot `gdb -p <root> -batch -ex "thread apply all bt"`. Shim unit tests: `RUST_MIN_STACK=64M`, `--skip tun`.
  Gaps: IPv6 rides the IPv4 machinery; `/proc/<pid>/fd` shows non-path fds as `anon_inode:[N]`.

## Cross-process shared memory and locks (all DONE; mechanism in the archive)

`RawMutex` = manual wait queue (32 slots, pointer-free) + cross-process `Event`s with `holder_pid`
dead-holder recovery (`bff1d0b`); waits are chunked to `LIVENESS_CHECK_INTERVAL` (2s) even for an infinite
wait, so recovery can ever run. A 128MiB (`SHARED_KERNEL_HEAP_SIZE`) `shared_kernel_arena_alloc` backs
`SharedArc<T>` (`GlobalState`, `Network`, pty/unix tables); `SLAB_ALLOC` stays per-process. `SharedArc::new`
shares only `T`'s inline bytes -- a `BTreeMap` has private-heap nodes, so shared registries are fixed-slot,
lock-free, atomic tables. Still per-process: `flock_registry`, `SafeZoneAllocator`'s `SpinMutex`. On Windows
every address/TID-based wait (`WaitOnAddress`, keyed events) is process-local; only a shared kernel object
crosses.

## Cross-process file visibility, identity, apt (2026-09-30)

- A file written by one host process is invisible to siblings until it exits (per-process writable layer).
  `syscalls/file_spill.rs` write-through-spills `SPILLED_PREFIXES` to
  `%TEMP%\litebox-spill-<rootpid>\<slot>.bin` (1024 slots); open/stat/access/getdents/unlink/rename refresh
  the local copy. Prefixes today (`file_spill.rs:10-18`): `/var/lib/apt/`, `/var/cache/apt/`,
  `/tmp/.config/chromium`, `/tmp/.cache/chromium`, `/root/.config/chromium`, `/root/.cache/chromium`,
  `/tmp/org.chromium.`. Fixes `apt-get update`. Extend the prefix list, not the mechanism;
  `%TEMP%\litebox-spill-*` is never cleaned up.
- Direction matters: a cross-process fork child receives the parent's exported writable layer AT SPAWN,
  while a child's own writes reach the parent only when it exits and `wait4` imports them
  (`litebox-forkwrite-*.tar`). So a fork carry of a fresh `/tmp` fd works and the same fd handed back over
  SCM_RIGHTS does not; `only_in_own_writable_layer` is how the carry path tells those apart.
- The fs layers check permissions against the calling task's fsuid/fsgid
  (`litebox::fs::set_effective_identity`); root bypasses rwx bits. `chown` is real, export/import carry owner
  and full mode. Anything walking the fs outside a syscall (export/import) must run inside
  `litebox::fs::with_root_identity`.
- The external fault watchdog only counts stalls after the VEH sets `litebox-fault-armed-<pid>`; before, it
  killed any idle method process (apt's sqv, exit 1 = "signal 9").
- `NETLINK_AUDIT` acks every `NLM_F_ACK` message; `getpriority`/`setpriority` exist (pam_limits aborts on
  ENOSYS); AF_INET6 answers `getsockname` etc. as v4-mapped `sockaddr_in6`.
- Fork children inherit cwd and full credentials (`task-state:` shim spec, fd `i32::MAX`); a child's
  deletions reach the parent as `.wh.` tar entries; only the newest Source pipe bridge of a read end drains
  (older siblings deadlocked dpkg-deb). `apt-get install -y file` completes.

## Containers and OCI

`litebox_packager --oci-image <ref> --output <tar>` pulls, merges, rewrites every ELF; the runner does the
same in memory (`.litebox-cache/`, keyed by `REWRITER_CACHE_VERSION`; `LITEBOX_OCI_USE_LAST_RESOLVED=1`
pins). Verified: `linuxserver/webtop:debian-xfce`/`ubuntu-xfce` ship XFCE, `alpine-mate` ships MATE,
`alpine-xfce` does not exist. For `--gui` DRM use `Xorg` with `modesetting`; for browser/selkies `Xvfb`.

## Closed -- do not re-attempt without a genuinely new approach

VEH_FRAME_STRIDE canary; `dev_bench`/`litebox_runner_snp` build failures; CoW-mmap performance; input
latency (pre-118th); presenter-split duplicate `SYN_REPORT`; GUI-protocol decision; `spawn_suspended`
stdio-handle bug; presenter-process split; ACK-stall-kill and port-8081 watchdog (2026-09-16);
"session-client death cascade" and `xfdesktop`-first client deaths (both were the driver cap).

## Docs and tooling map

- Archives, newest first, under `docs/`: `AGENTS_ARCHIVE_2026-09-29.md` (118th), `_2026-09-28.md`
  (4th-116th), `_2026-09-23.md`, `_2026-09-22.md`, `_2026-09-18.md`, `_2026-09-17.md`, `_2026-09-16.md`,
  `_2026-09-15.md`, `_2026-09-10.md`, `_2026-09-03/05.md`.
- Fork: `docs/track-b-fork-fix-progress.md`, `advisor/ADVISORY-002-d-zero-fork.md`,
  `advisor/ADVISORY-001-fundamentals.md`; VEH: `docs/veh-exception-handler-design.md`; also
  `diag-timeline-field-semantics.md`, `premade-library-research.md`, `drm-dumb-buffer-ioctl-reference.md`,
  `macos.md`; `advisor/probes/` (decode_frame.py, symbolize_litebox_crash.py, MEASUREMENT-PITFALLS.md,
  DISK-HYGIENE.md); `.gm/memories/` (superseded).
- Helpers in `.wfgy/`: `logscan.py` (ANSI-stripping term scan -- gm codesearch skips these logs as binary),
  `lockstall.py` (aggregates `[diag-lockstall]`), `scan_waiters.py` (arena `dd` -> `WaiterQueue` slots),
  `symstk.py`/`symstk2.py` (llvm-symbolizer, 7/30 frames), `cdbsweep.ps1`, `cpuprof.ps1`.

## Chromium (2026-10-03) -- renders a page with its OWN sandbox active

**MILESTONE (chr21/chr22)**: `.wfgy/guest2.ps1 -Script .wfgy/chr15.sh -Run chr21 -Secs 240` (webtop,
`LITEBOX_PROCESS_FORK=1`, NO `--no-sandbox`) prints
`<html><head></head><body><h1>hello from litebox</h1><p>sandboxed chromium rendered this</p></body></html>`
plus `CHROME_PIPELINE_DONE rc=0`; 6x `Activated seccomp-bpf`, `Linux.SandboxStatus`=106
(UserNS+NetNS+TSYNC+AMD64), ZERO `No usable sandbox!`, ZERO `Sanity checks are failing`. The old "zygote
needs `Credentials::CanCreateProcessInNewUserNS()`" FATAL does NOT fire for a non-root uid; only the root
path still dies (crbug 638180). Desktop (windowed): `.wfgy/chrdesk.ps1 -Run <n> -MaxSeconds N`.
- **rc=133 (SIGTRAP) is a `HOME` problem, not a litebox one.** Chromium runs as uid 911 (`setpriv --reuid
  911`) and derives its crashpad database from `HOME`; with `HOME=/config` (root-owned 0755)
  `chrome_crashpad_handler` is exec'd with EIGHT arguments and no `--database`, answers `--database is
  required`, the client's handshake read fails and the browser dies on a CHECK. With a writable
  `HOME=/tmp/cuhome` the identical command line exits 0 and dumps the DOM (reproduced headlessly,
  `.wfgy/cpad1.sh`). `--crash-dumps-dir` is NOT the lever: it does not exist in Debian's build.
- **`--publish` works with an ordinary fork-child server**: a background `python3 -m http.server 8081`
  answered the host 200/61 bytes seven times (pub2). So a published port that hangs is NOT automatically a
  cross-process-fork/gateway bug -- ask the guest to curl its own `127.0.0.1:<port>` first. (pub2 phase B:
  after an `exec` of the ROOT process the host listener stops accepting -- published ports are root-process
  state, so `exec` in the root kills them.)
- What unblocked it: `SHARED_UNIX_CONN_CAPACITY` 1024 -> **4096** (`7d2a6a7`); chr20's census read
  `occupied=1024 held_live=1024`, 303 refusals ALL `reason=shared unix connection table full`. To fit one
  power-of-two arena region the slot shrank ~14.8KB -> ~7KB (`SHARED_UNIX_CONN_BUF` 4096->2048,
  `RING_FD_MAIL_ENTRIES` 4->2); the pty keeps 4096 via `PTY_RING_BYTES` + `PtyRing<Platform>`. Arena 64 ->
  128MiB is free (`SEC_RESERVE`, committed on demand).
- An SCM_RIGHTS carry the receiver never adopts leaks the sender's slot hold forever:
  `UnixSocket::release_unadopted_carry` now runs on every discard path (`net.rs` EMFILE/rebuild
  error/MSG_CTRUNC, `file.rs::rebuild_carried_unix`, `UnixSocket::recvfrom`).
- `seccomp(2)`/`prctl(PR_SET_SECCOMP)` are real: a classic-BPF interpreter (`syscalls/seccomp.rs`) on the
  raw syscall number at the top of `Task::do_syscall`; mode, `no_new_privs` and the filter stack live on
  `Process` (TSYNC free, all three survive clone and execve). `SECCOMP_RET_TRAP` must RETURN THE SYSCALL
  NUMBER (`ad2659f`): `syscall_rollback` leaves `rax = orig_ax` and Chromium's `Trap::SigSys` asserts
  `si_syscall == SECCOMP_SYSCALL(ctx)`. Traps/kills log `seccomp: SECCOMP_RET_*`; errno verdicts are
  deliberately not logged. ~19 SIGSYS on `sched_getaffinity` per run are Chromium's OWN trap handler.
- Chromium must run NON-ROOT (`setpriv --reuid 911 --regid 911 --init-groups`) and `--user-data-dir` must be
  `chmod 777` when a root shell created it (else `SingletonLock: Permission denied` = exit 21).
- The carry path (SCM_RIGHTS and fork share ONE, `6beb669`): `scm_carry_spec` returns
  `Result<Option<String>, &'static str>` and `net.rs` logs `reason=` with every refusal -- READ THAT FIELD.
  A connected endpoint, an unbound socket and a listener cross; a bound-but-unconnected socket, a connect in
  progress and a bound/connected DATAGRAM socket are refused EOPNOTSUPP on purpose. Only a `Presence` hold
  is abandoned; a `Conn` hold ships with the spec. A file that exists only in the SENDER's writable layer
  goes out as `T|` (content snapshot), everything else as `F|` (must stay the SAME inode on both sides). Do
  NOT fix this class by adding `/tmp/` to `SPILLED_PREFIXES`. Repro `.wfgy/scmring.sh`;
  `.wfgy/scmlisten.sh` (NOT yet run).
- Fixed on the way: fork children keep committed `VM_OWN_FORK_PADDING` (`067367b` -- was the renderer AV
  `error_code=0x4`); cross-process `/proc/<pid>` via `procfs::set_pid_known_fn`+`statm`; huge `PROT_NONE`
  mmaps reserve address space only (`RESERVE_ONLY_THRESHOLD`); guest `int3` reaches the VEH;
  `/dev/shm`+memfd travel by named section; tar mtimes preserved; `SO_PROTOCOL`/`SO_DOMAIN`.
- Still open: (1) the launcher thread SERIALISES cross-process spawns, so children can hit the 15s "no
  connection" self-termination -- native COW fork or a parallel launcher is the lever; (2)
  `--single-process` dies on a Chromium CHECK in
  `RenderProcessHostImpl::GetProcessHostForSiteInstance`; (3) ~1 per run `rebuilding a carried SCM_RIGHTS fd
  failed errno=ENOENT spec=F|.../Local Storage/leveldb/LOG`; (4) `Vmem::duplicate` still skips
  `VM_OWN_FORK_PADDING` (the SAME-process fork); (5) GWP-ASan `MapRegion` EEXIST occasionally.
- **UNRESOLVED CONTRADICTION**: `9014df7` reported "chromium headless prints example.com" -- a page WAS
  produced once, after unix `EPOLLOUT` stopped firing when the blocked message does not fit. Diff
  `9014df7..HEAD` for unix/socket behaviour before assuming anything about the Mojo path.

## Selkies / host-browser visibility (2026-10-03)

The remaining gap to "visible from the browser" is selkies; several runs say where it is and where it is NOT:
- The `--publish` path is HEALTHY, including large responses: the host fetched selkies' own `index.html`
  (786B), `assets/index-*.js` (**650KB, 0.53s**), css, `manifest.json`, `icon.png`, `/api/status`, all 200.
- The host BROWSER loads the app: CDP navigation to `http://localhost:8081/` succeeds (60s was too tight,
  120s is not), 15/15 requests 200, zero console errors, and selkies logs `INFO:ws:Client ('10.0.0.1',
  49162) connected`. It then sits on "Waiting for stream..." and the canvas stays black.
- **SELKIES 2.0.0 BINDS 8080, NOT 8081 -- the old "selkies stalls at startup" symptom was a PORT MISMATCH**
  (selk26): with everything else healthy it logged `INFO:server:Selkies server running on
  http://0.0.0.0:8080`, while every probe, the `--publish 8081:8081` mapping and the host browser aimed at
  8081 -- the chr6/selk6/selk8 refusals are explained by this alone. `CUSTOM_WS_PORT` is a 1.x name 2.0.0
  ignores; **`--port=8081` is now passed explicitly**. Read the port out of selkies' OWN "running on
  http://..." line before trusting any probe.
- **SELKIES 2.0.0 ENABLES BASIC AUTH BY DEFAULT AND, WITH NO PASSWORD SET, STOPS ITS OWN SERVER** (selk12):
  `ERROR:server:Basic authentication is enabled but no password was set`, then
  `INFO:server:Stopping service: websockets` -- the port NEVER LISTENS. That is the entire selk6/selk8/
  selk11/selk12 "wedges during its own startup" symptom: refusal-by-config, not a stall. Always pass
  `--enable-basic-auth=false`, or set `PASSWORD`.
- Two harness traps: selkies needs `DISPLAY` exported (it exits silently without one), and a log file it
  writes under `/tmp/` is INVISIBLE to the shell that launched it -- `wc -l` over a running server's log
  reads 0. Pipe the child's output: `( selkies --debug 2>&1 | sed 's/^/[selk] /' ) &`.
- **MEASUREMENT ERROR, corrected (aio9)**: four server arms on one guest (uvloop+`asyncio.start_server
  (sock=)`, uvloop+`start_server(host,port)`, uvloop+aiohttp `SockSite`, plain `SelectorEventLoop`), each
  probed twice: **8/8 `code=200`**. So guest loopback TCP, uvloop's epoll, libuv's accept and aiohttp all
  work. The runs that "proved" otherwise (aio5-aio7) passed an `(reader, writer)` callback to
  `create_server`, which takes a no-arg PROTOCOL FACTORY -- every accept raised `TypeError`. **Set
  `loop.set_exception_handler` in every guest probe script -- an exception there is invisible and reads as a
  litebox hang.** Also: the litebox log clock starts at runner start and a guest script's `t0` is ~8s later,
  so litebox t=28.9 IS guest t=21.
- **selkies here is 2.0.0, a rewrite with DIFFERENT flags** (selk7 printed the real `--help`).
  `--clipboard-enabled` does not exist; the real names are `--enable-clipboard`, `--printing-enabled`,
  `--print-spool-path`, `--gamepad-enabled/--audio-enabled/--microphone-enabled/--webcam-enabled`, `--mode`,
  `--enable-basic-auth`, `--unix-socket`, `--port`, `--addr`, `--web-root`. Read `selkies --help` first.
- **PRINTING IS NOT THE CAUSE (selk8, both arms wedged identically)** -- `start_server()`'s printing block
  is innocent. A/B on ONE port is useless (`kill -9` on a wedged cross-process child does not reap it ->
  `Address already in use`). **selk9** cleared the two printing primitives (watchdog inotify Observer runs,
  `asyncio.create_subprocess_exec` works) but the same spawn WITH `preexec_fn` fails
  (`Exception occurred in preexec_fn` -- selkies' `_die_with_parent` `ctypes prctl(PR_SET_PDEATHSIG)`,
  printing.py:239), so cupsd can never start under litebox today.
- Guest-side `ps` is useless here (each cross-process child's `/proc` lists only itself); a host-side process
  is identified by its THREAD NAMES: `litebox-guest-pid<N>`.
- Guest app source is in the cached layer tars: selkies 2.0.0 is `sha256_669f2c..._v2.tar` under `lsiopy/`
  (an older 1.x tree sits in `sha256_3e6fd1..._v1.tar` -- do not read that one by mistake).
- Cheapest repro of the current wedge: `.wfgy/chrdesk_lock.ps1 -Run chrD5 -MaxSeconds 660 -Script
  .wfgy/chrdesk3.sh` (adds `LITEBOX_DIAG_LOCKSTALL=1` + `LITEBOX_DIAG_WAIT_DUR=1`, see
  "Frozen-in-`close(2)`"). `.wfgy/selk6.sh` is the no-DE selkies-only variant.
