# litebox -- current state (2026-10-03, compacted; 119th-120th pass)

CURRENT-STATE picture: what works, what is broken, what to do next. Every claim carries a commit sha or
`file:line` so the next session re-verifies instead of re-deriving; a claim nobody can point at, or one a
later commit superseded, is deleted. Detail (4th-118th passes) is in `docs/AGENTS_ARCHIVE_*.md` -- a trail,
never a starting point. Single source of truth for standing rules.

## Where things stand

**A real XFCE desktop runs in a real browser and is usable**: `Xvfb` + `xfce4-session` (5 clients) +
`selkies` (x264, MIT-SHM) in litebox, host proxy `--publish 8081:8081`; `DE_UP` in 25-40s. Verified via real
Chrome CDP (2026-09-30, `.wfgy/pass118_fin11.*`, `.gm/witness/stack118_t*.png`): Applications menu,
xfce4-terminal (prompt, `tty`, job control, pipelines, `su`, DNS), Mousepad, Settings, Thunar,
`ls /proc/self/fd`, `apt-get update`+`install`; 12+ min soak, 30s probes all 200, zero `resetting Network`,
24 host processes, 3.3GB WS. Typing drops chars when the host is starved; chrome-devtools `click` cannot hit
injected overlays -- use `Alt+Tab`/keyboard.

Fixed (newest first; mechanism in the archive unless noted):
- `a1423ee` a carried "child writes" pipe is a local pipe + a pump, so the guest's `write(2)` returns
  before the bytes reach the parent -- a bulk producer that exits at once lost its tail (`dd bs=1024
  count=200 | wc -c` = 151552 of 204800; three stages 81920). The child's exit now waits for its write
  pumps to drain (bounded 5s). All seven shapes exact after.
- `30d8f43` a `Source` pipe bridge waited for `owners()==1` before forwarding a byte, but HOLDING a pipe
  end is not READING it: a wrapper (`timeout`/`env`/`nohup`/`setpriv`) keeps its stdin open to hand to
  the child it forks, so `owners()` never fell and `at_eof()` stayed false while the bytes sat buffered
  -- deadlock (`printf x | timeout 12 sh -s` was rc=124 25/25). `ForkPipeBridge::pending_bytes` (FIONREAD)
  now breaks the wait when a non-zero count has sat unchanged 500ms: data nobody is consuming.
- `7d2a6a7` `SHARED_UNIX_CONN_CAPACITY` 1024 -> 4096, and every unadopted-carry discard path releases the
  sender's slot hold: **chromium renders a page with its OWN sandbox active** (Chromium section). `ad2659f`
  `SECCOMP_RET_TRAP` returns the syscall number, a connected endpoint's `Conn` hold ships with the carry
  spec, `SO_PROTOCOL`/`SO_DOMAIN` answered.
- `067367b` fork children keep committed fork padding (`VM_OWN_FORK_PADDING` in the copy-group filter, kept
  committed in `Vmem::adopt`); cross-process `/proc/<pid>`; `SECCOMP_RET_TRAP` payload + syscall number;
  no-restorer frames refused; AV heals bounded (`MAX_AV_PATH_HEALS`).
- `a2f3eb9`+`7d2a6a7` `SHARED_UNIX_CONN_CAPACITY` 64 -> 1024 -> 4096 (`unix.rs`), arena-backed. `9412184`
  writable-layer-only file carries by content (`T|`). `6beb669` SCM_RIGHTS + fork share one unix carry path.
- `19eab93` PID namespaces; crashpad (`yama/ptrace_scope`, `PR_SET_PTRACER`, SCM_CREDENTIALS);
  `CLONE_FS`/`CLONE_FILES` clones stay same-process. `8478b15` fork children keep `PROT_NONE` reservations
  (`reserve_pages_without_commit`).
- `3bfe283` `CLONE_NEWUSER` credentials cross fork; `VM_MAY_ACCESS_FLAGS`. `883cab4` user namespaces (id
  maps, `setgroups` first) + `Backend::services_own_writes`. `e608959` real `chroot(2)`. `2bb71cb` real
  `seccomp(2)` (classic-BPF on the raw nr at the top of `Task::do_syscall`).
- `668d084` `idle_trim.rs` (`LITEBOX_IDLE_TRIM=0` off). `f271ed2` flat rootfs index, regexless OCI parse.
  `d428acd` `with_root_identity` also takes the per-thread root guard. `a2ebff6` CLOEXEC survives a fork
  carry for pipe ends; inotify fds not carried. `748c4e5` registry + record locks out of the shared arena.
- `9014df7` unix `EPOLLOUT` only when the blocked message fits -- **reports `chromium headless prints
  example.com`**, not reproduced since. `582cc4b` chromium profile dirs use the shared spill. `f36621a` one
  `VirtualQuery` per region: fork 3-5s -> ~0.5s.
- Older: `a42d9d0` pipeline EOF; `d944c66` net_lock not orphaned on exit; `f594693` mutable `Credentials`
  + `IP_RECVERR`; `2b7d7df` TCP `mark_peer_closed()`; `88f632f` device write; `f0ecbb0` `getsockname`;
  `290d4d4` `mprotect` rounding; `3ee1ce7`/`bff1d0b`/`a61ed74` lock dead-holder recovery; `0cda0ec` lazy
  range + `epoll` re-poll; `d383f90`/`d1dae93` pty; `820c2d6` watchdog arming; `bb518ca` per-fork env vars
  consumed by `take_fork_env` -- **any new per-child env var needs a remove after adoption**.
- Memory (`b11f674`, `6fd27b7`, `d07a7f9`, `f271ed2`, `668d084`): fork child 41MB -> 8MB resident; stack
  private resident 2246MB -> 1222MB; idle trim WSsum 2.8GB -> ~0.4GB. Beware root-vs-child heap layout
  differences exposing shared-struct host pointers (`bootstrap_process`, `6fd27b7`).

## Open, in rough priority order

1. **Host memory is the binding constraint, not a litebox logic bug.** Left: selkies/Xvfb heaps (150-250MB
   private RW, legitimate), guest `PROT_NONE` reservations are committed, an idle process still burns 1-3%
   CPU. Diagnose with `LITEBOX_DIAG_ALLOC_STACK=1` (symbolize with `llvm-symbolizer`) and
   `LITEBOX_DIAG_MEM_BREAKDOWN=1`; sample with `.wfgy/memsamp.ps1`, `.wfgy/wsmap.ps1 -ProcId`.
1b. Chrome (the user's own, ~6GB) and other host apps leave 0.3-2GB free, so full-stack runs die on the
   driver's `KILL low memory` guard (`avail<120`) before `DE_UP`; check
   `Get-Counter '\Memory\Available MBytes'` first. Gate in `pass118_full_err.ps1` is 1000MB.
2. Verify in the full stack (committed, not yet seen in a browser): `3e1ef47` SCM_RIGHTS over a
   cross-process unix connection (thunar's D-Bus call passes a dup of stdin; the refusal closed GDBus's bus).
   `b012910` lazy file map dropped stale entries over an execve'd fork child's libraries. `08ae94f`
   `/proc/<pid>/fd`. Repro `.wfgy/th1.ps1 -Run th1|th2|th3`.
3. Selkies: one client per instance, no slot reclaim on reload.
4. Native kernel-COW fork on Windows (`.gm/prd.yml` `native-kernel-cow-fork`); writable layer shared across
   processes; AF_UNIX exhaustion silent (`SharedUnixAddrPresenceTable` 256 slots, keys >108 bytes);
   `flock_registry`/`drm`/`evdev` per-process; `timerfd`/`signalfd` uncarriable; fixed-address (non-PIE) exec
   from a same-process vfork child collides (gcc). macOS/Linux builds unverifiable on this host.

## How to run and drive it (harness lessons that cost sessions)

- Cheap repro: `target/release/litebox_runner_linux_on_windows_userland.exe -Z --oci-image docker.io/
  library/debian:stable-slim -- /bin/bash -c '<script>'` (or the cached `webtop:debian-xfce`).
  **PowerShell, never Git Bash** for guest paths (Git Bash rewrites `/abs/guest/paths`); single quotes only
  inside `-c`; redirect with `cmd /c "... < script.sh > out 2> err"` (`Start-Process` redirection kills the
  runner; `*>` logs are UTF-16LE, word-wrap at ~116 chars). Confirm the exe mtime postdates the newest
  commit. **Build only `cargo build --release -p litebox_runner_linux_on_windows_userland`; the whole
  workspace does not build on Windows.**
- Full stack: `.wfgy/pass118_full.ps1 -Run <name> -MaxSeconds N` (+`.sh`); waits for 2.5GB free RAM (the log
  appears late) and **terminates every `litebox_runner` at its cap -- a stale older driver kills a newer
  run** (cost two sessions). Kill leftover driver powershells first; give a cap longer than needed. Variants
  `pass118_full_err.ps1` (adds `litebox_diag::stderr_capture=debug`), `pass118_full_a11y.ps1`.
  `acquire_boot_lock()` refuses a second boot while an orphaned run lives.
- Browser: `mcp__chrome-devtools__*` for real CDP input. `click` needs a uid, so inject a fixed
  `pointer-events:none`, opacity .01 `<button id=probe>` at the wanted x,y and click that (the real mouse
  event lands on the video). `Control+Alt+t` opens Terminal (allow 30s); gm `cdp` is JS-eval only.
- **Guest scripts must be LF.** A CRLF `.wfgy/*.sh` turns every `\`-continued line into its own
  command (`/bin/bash: line 55: +iglx: command not found`, boot exits 127 immediately) -- the failure
  looks like a litebox startup bug and is not one. Git's `core.autocrlf` reintroduces it; check any
  script that has not been run since.
- Cheap guest repro with no file in the guest: `.wfgy/guest2.ps1 -Script <sh> -Run <name> -Secs <n>`
  (base64's the script onto the command line; sets LITEBOX_PROCESS_FORK=1, LITEBOX_LAZY_FILE_MAP=1,
  LITEBOX_OCI_USE_LAST_RESOLVED=1; `-ExtraEnv "K=V;K2=V2"` overrides). **It kills every `litebox_runner*`
  at start, so runs must be sequential**: starting a second run killed the live one and then could not boot
  on the held lock.
- Diagnostics: `cdb -pv -p <pid> -xd av -xd sse -c "~*kb 25; qd"`, symbolized with
  `llvm-symbolizer --obj=<exe> --relative-address` (`.wfgy/symstk.py`; release ICF mislabels callers -- build
  without `--release` when the caller matters); `litebox_diag::{process_timeline,socket_read,
  unix_conn_teardown,stderr_capture}=debug`; `LITEBOX_DIAG_NO_EXTERNAL_FAULT_WATCHDOG=1`/
  `LITEBOX_DIAG_NO_FAULT_WATCHDOG=1` before any `cdb` attach. A cross-process child whose exit code lacks
  the `0xC0DE` marker decodes as SIGKILL (`decode_cross_process_exit_status`), so `rc=137` also means "host
  process died some other way".
- **Before blaming litebox for a death or stall, correlate it with the harness's own time caps, kill loops
  and the host's free RAM / pages-per-sec first.**
- **The image's `Xvfb` is XLibre now and ABORTS when `/tmp/.X11-unix` already exists with the wrong
  mode**: its `_XSERVTransmkdir` warning uses `%o` and `os/log.c`'s `vpnprintf` has no `o` directive
  (`BUG: ../os/log.c:620 in vpnprintf()` + a backtrace). Every X client then fails with `unable to open
  display :1`, and cross-process with `connect_cross_process: listener in another process never accepted
  within SHARED_UNIX_CROSS_CONNECT_TIMEOUT`. So `mkdir -p /tmp/.X11-unix && chmod 1777 /tmp/.X11-unix`
  BEFORE starting Xvfb (now in `.wfgy/{xv1,chrdesk,pass118_full}.sh`); `webtop_stack.sh` never creates the
  dir, which is why the s6-driven stack was never hit. A dead X server looks exactly like a litebox unix
  socket bug -- check the server's own output first.
- **Run dbus-daemon non-forking** (`dbus-launch` daemonizing breaks connects); for XFCE use
  `xfce4-session`. `.wfgy/webtop_stack.sh` is embedded in `.wfgy/webtop_seed.tar` (re-tar after edits).
  Readbacks inside a boot script use `$( )`/pipes, not `cmd > /tmp/f` + a sibling's read.
- **Repo hygiene**: layer tars, frame dumps, debug logs never in git (`.wfgy/` is ignored); no test files;
  commit as lanmower only.

## Standing lessons and hard constraints

- **No WSL/hypervisor ever.** Never `bcdedit /debug on` without a kernel debugger. A process that resists
  `Stop-Process` needs WMI `Terminate`. `cdb` must use `-pv`/`qd`, never bare `q`.
- **Never run two full-stack verifications concurrently**; watch `Available MBytes`, kill on a falling trend.
- **Guest-reachable code returns an errno, never a panic** (the host process IS the whole guest session).
  Refusal errno is API contract: EPERM lets callers degrade, EINVAL/ENOSYS fails them hard.
- **`LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check.** Never time litebox with one host
  process per datapoint (spawn costs 1.6-2.3s). Never trust a container tag name for its contents.
- **A `TypedFd` index is valid only against the `Descriptors` that inserted it**; shared-memory structs must
  hold no process-relative pointers (`Network`, `Pipes`, `FutexManager` were all this bug). A mutable table
  on the `SharedUnixAddrPresenceTable` pattern needs every write path mirrored to the shared side.
- **Cross-process fork (`LITEBOX_PROCESS_FORK=1`)**: a genuine `D==0` fork (`spawn_cross_process_fork_child`,
  `litebox_platform_windows_userland/src/lib.rs:13609`); pipes/regular files/eventfds/pty slaves+masters/unix
  sockets carried, cloexec+pty fds dropped, only unrecoverable kinds refuse; `live_cross_process_fork_children`
  caps concurrent children at 6. A run took this path only if `[process_fork_diag] task-resume-probe` lines
  exist. Cross-process `kill()` goes through `GlobalState::process_table` (`syscalls/signal/xproc.rs`);
  `fork_verify.rs` healing is Windows-only. **The env var is PRESENCE-CHECKED** (`std::env::var_os`):
  `LITEBOX_PROCESS_FORK=0` still ENABLES it -- unset it for the same-process path. A `socketpair` fd is
  carried (`raw_fd_is_addressless_unix_socket_pair`). A child's guest pid is the parent's `child_tid`; its
  Windows pid is `winpid=`/`host_pid`.
- **`chroot(2)` is real (e608959)**: `FsState.root` is shared by `CLONE_FS`, so a chroot on any task sharing
  that state (pthread, `CLONE_FS` clone) roots all of them, parent included; `cwd` is root-space, dirfd-
  relative paths stay unrooted (an fd opened before the chroot still escapes). Do NOT test `CLONE_FS` with a
  raw `clone(CLONE_VM|CLONE_VFORK|CLONE_FS)` from CPython: the parent never resumes, the process dies 139;
  use pthreads.
- **Logs**: default `warn,...fork_verify=error`; prefer dedicated low-overhead targets over blanket module
  debug (`syscalls::file=debug` floods 50MB/s). A boot whose log stops is usually a dead root runner (a
  cross-process child has the bare 77-char command line). **Verbosity comes from `LITEBOX_LOG`, not
  `RUST_LOG`.** Disk hygiene: one duplicated warn produced a 1GB Chromium log in 2.5 min -- sample repeated
  messages (stride counter) or cap them (`MAX_AV_PATH_HEALS`, `AV_HEAL_LOG_SAMPLE_STRIDE`).

## Linux runner (`litebox_runner_linux_userland`, merged from branch `claude/modest-feynman-3zpzop`)

Cloud sessions are Linux; there the `webtop:debian-xfce` rootfs (`--initial-files rootfs.tar
--rewrite-syscalls --uid 0 --gid 0 --pid1 --tun-device-name tun0 ... /bin/sh /init`) boots under the Linux
runner: s6, Xvfb, xfwm4/panel/xfdesktop, nginx, pulseaudio, dbus, Selkies; a host browser at
`http://10.0.0.2:3000` (TUN, host 10.0.0.1) drives it. Harness: `tools/webtop/`. Not re-verified after the
merge with the Windows line.
- Native fork (`has_native_fork()==true`): shared-arena fork, pool-backed shared memory; hooks are
  `waitpid`/`waitid(WNOWAIT)`; `SYS_wait4`/`SYS_waitid` seccomp-allowed (a blocked host syscall answers
  EINVAL and looks like a hang); `exit_native_fork_child` ends the child with the guest status. A
  native-fork child COW-copies every private kernel structure, so shared state must live in the arena. The
  thread-based fork corrupts guest memory on Linux -- not a substitute.
- Per-process `/proc/self` identity, `/proc/<pid>/{task,oom_score_adj,environ,fd}`; real pty line
  discipline; SCM_CREDENTIALS; shared-mapping coherence; per-thread fs "act as root" guard; network worker
  never blocks on a per-descriptor lock (`iter_nowait`); runner panic = `_exit(134)`; big read-only private
  file maps are ONE shared object; freed heap >= 1MiB returns pages (`MADV_REMOVE`). Plateau ~9GB (cgroup
  ~14GB).
- Added: inotify, `MSG_PEEK` on unix+inet (used to consume bytes; broke TLS), xattr stubs, tar mtimes,
  `mincore`, `copy_file_range`, `splice`, `rt_sigtimedwait`, `rt_(tg)sigqueueinfo`, `O_TMPFILE`,
  NETLINK_ROUTE dumps, `pidfd_open`/`waitid(P_PIDFD)` (also reaps cross-process children), `getrusage`/
  `mlock`, ptrace answers EPERM.
- Dead-holder recovery for `RwLock` and platform `RawMutex`: a waiter blocked 2s checks `tkill(tid,0)`
  (`set_thread_id_fn`/`set_thread_alive_fn`); up to 4 readers tracked. A panic holding the layered-fs root
  write lock hangs every later `open`. Debug kit: `LITEBOX_DIAG_FAULT=1 LITEBOX_PRINT_EXE_BASE=1` +
  frame-pointer build; `LITEBOX_DIAG_BIGALLOC=1`; stalled boot `gdb -p <root> -batch -ex "thread apply all
  bt"`. Shim unit tests: `RUST_MIN_STACK=64M`, `--skip tun`. Gaps: IPv6 rides the IPv4 machinery;
  `/proc/<pid>/fd` shows non-path fds as `anon_inode:[N]`.

## Cross-process shared memory and locks (all DONE; mechanism in the archive)

`RawMutex` = manual wait queue + cross-process `Event`s with `holder_pid` dead-holder recovery (`bff1d0b`).
A 128MiB (`SHARED_KERNEL_HEAP_SIZE`) `shared_kernel_arena_alloc` backs `SharedArc<T>` (`GlobalState`, `Network`, pty/unix tables);
`SLAB_ALLOC` stays per-process. `SharedArc::new` shares only `T`'s inline bytes -- a `BTreeMap` has
private-heap nodes, so shared registries are fixed-slot, lock-free, atomic tables. Still per-process:
`flock_registry`, `SafeZoneAllocator`'s `SpinMutex`. On Windows every address/TID-based wait
(`WaitOnAddress`, keyed events) is process-local; only a shared kernel object crosses.

## Cross-process file visibility, identity, apt (2026-09-30)

- A file written by one host process is invisible to siblings until it exits (per-process writable layer).
  `syscalls/file_spill.rs` write-through-spills `SPILLED_PREFIXES` paths to
  `%TEMP%\litebox-spill-<rootpid>\<slot>.bin` (platform `spill_*`, index in `GlobalState.shared_file_spill`,
  per-slot generation); open/stat/access/getdents/unlink/rename refresh the local copy. Prefixes today
  (`file_spill.rs:10-18`, 1024 slots): `/var/lib/apt/`, `/var/cache/apt/`, `/tmp/.config/chromium`,
  `/tmp/.cache/chromium`, `/root/.config/chromium`, `/root/.cache/chromium`, `/tmp/org.chromium.`. Fixes
  `apt-get update`. Extend the prefix list, not the mechanism; `%TEMP%\litebox-spill-*` is never cleaned up.
- Direction matters: a cross-process fork child receives the parent's exported writable layer AT SPAWN (its
  `[process_fork_diag] task-resume-probe (child): guest fd N reopened on /tmp/...` lines are that layer being
  readable), while a child's own writes reach the parent only when it exits and `wait4` imports them
  (`litebox-forkwrite-*.tar`). So a fork carry of a fresh `/tmp` fd works and the same fd handed back over
  SCM_RIGHTS does not; `only_in_own_writable_layer` is how the carry path tells those apart.
- The fs layers check permissions against the calling task's fsuid/fsgid
  (`litebox::fs::set_effective_identity`, set at every syscall entry); root bypasses rwx bits. `chown` is
  real, export/import carry owner and full mode. Anything that walks the fs outside a syscall (export/import)
  must run inside `litebox::fs::with_root_identity` (`d428acd`: it must take the per-thread root guard too).
- The external fault watchdog only counts stalls after the VEH sets the `litebox-fault-armed-<pid>` event;
  before, it killed any idle method process (apt's sqv, exit 1 = "signal 9").
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
On the thread-based fork path XFCE needs guest-side
`--env GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0` and selkies needs
`--clipboard-enabled=false` (both moot under `LITEBOX_PROCESS_FORK=1`).

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
  `advisor/ADVISORY-001-fundamentals.md`; VEH: `docs/veh-exception-handler-design.md`.
- `docs/diag-timeline-field-semantics.md`, `docs/premade-library-research.md`,
  `docs/drm-dumb-buffer-ioctl-reference.md`, `docs/macos.md`; `advisor/probes/` (decode_frame.py,
  symbolize_litebox_crash.py, MEASUREMENT-PITFALLS.md, DISK-HYGIENE.md); `.gm/memories/` (superseded).

## Chromium (2026-10-03) -- renders a page with its OWN sandbox active

**MILESTONE (chr21/chr22)**: `.wfgy/guest2.ps1 -Script .wfgy/chr15.sh -Run chr21 -Secs 240` (webtop,
`LITEBOX_PROCESS_FORK=1`, NO `--no-sandbox`) prints
`<html><head></head><body><h1>hello from litebox</h1><p>sandboxed chromium rendered this</p></body></html>`
plus `CHROME_PIPELINE_DONE rc=0`; 6x `Activated seccomp-bpf`, `Linux.SandboxStatus`=106
(UserNS+NetNS+TSYNC+AMD64), `ZygoteMain: initializing 0 fork delegates`, ZERO `No usable sandbox!`, ZERO
`Sanity checks are failing`, ZERO `cannot cross a process boundary` refusals. The old "zygote needs
`Credentials::CanCreateProcessInNewUserNS()`" FATAL does NOT fire for a non-root uid; only the root path
still dies (crbug 638180). Desktop (windowed, browser-visible): `.wfgy/chrdesk.ps1 -Run <n> -MaxSeconds N`
with `.wfgy/chrdesk.sh`.
- What unblocked it: `SHARED_UNIX_CONN_CAPACITY` 1024 -> **4096** (`7d2a6a7`). chr20's census read
  `occupied=1024 held_live=1024 one_host=1009 multi_host=15`, 303 refusals ALL `reason=shared unix
  connection table full`. To fit one power-of-two arena region the slot shrank ~14.8KB -> ~7KB
  (`SHARED_UNIX_CONN_BUF` 4096->2048, `RING_FD_MAIL_ENTRIES` 4->2); `SharedByteRing` is const-generic over
  `RING_BYTES` and the pty keeps 4096 via `PTY_RING_BYTES` + the `PtyRing<Platform>` alias (a const generic
  argument must be a type or a BRACED const -- unbraced path is E0573). Arena 64 -> 128MiB is free: the
  reservation is `SEC_RESERVE` and `shared_kernel_arena_alloc` commits on demand.
- An SCM_RIGHTS carry the receiver never adopts leaks the sender's slot hold forever.
  `UnixSocket::release_unadopted_carry` now runs on every discard path: `net.rs` (EMFILE, any rebuild error,
  and fds dropped by MSG_CTRUNC are closed), `file.rs::rebuild_carried_unix` (failure), and
  `UnixSocket::recvfrom` -- a read/recv cannot hand out ancillary data but still consumed the message, so it
  takes `global` and releases every `AnyDupFd::Carried` it drops.
- `seccomp(2)`/`prctl(PR_SET_SECCOMP)` are real: a classic-BPF interpreter (`syscalls/seccomp.rs`) on the
  raw syscall number at the top of `Task::do_syscall`; mode, `no_new_privs` and the filter stack live on
  `Process` (TSYNC free, all three survive clone and execve) and a fork child rebuilds them from the
  `task-state` spec. `SECCOMP_RET_TRAP` must RETURN THE SYSCALL NUMBER (`ad2659f`): `syscall_rollback` leaves
  `rax = orig_ax` and Chromium's `Trap::SigSys` asserts `si_syscall == SECCOMP_SYSCALL(ctx)`, else it abandons
  the trap with "Sanity checks are failing after receiving SIGSYS." Traps/kills log `seccomp: SECCOMP_RET_*`;
  errno verdicts are deliberately not logged (Chromium takes hundreds per second on purpose). ~19 SIGSYS on
  `sched_getaffinity` per run are Chromium's OWN trap handler: expected, harmless.
- Chromium must run NON-ROOT (`setpriv --reuid 911 --regid 911 --init-groups`) and `--user-data-dir` must be
  `chmod 777` when a root shell created it (else `SingletonLock: Permission denied` = exit 21).
- The carry path (SCM_RIGHTS and fork share ONE, `6beb669`): `scm_carry_spec` returns
  `Result<Option<String>, &'static str>` and `net.rs` logs `reason=` with every refusal -- READ THAT FIELD
  before assuming which state Chromium's sockets are in. A connected endpoint, an unbound socket and a
  listener cross; a bound-but-unconnected socket, a connect in progress and a bound/connected DATAGRAM socket
  are refused EOPNOTSUPP on purpose. Only a `Presence` hold (listener) is abandoned; a `Conn` hold ships with
  the spec (`ad2659f`) because it IS the mechanism -- it counts the sender's host pid so the slot cannot be
  reclaimed between `sendmsg` and adoption, and `from_fork_spec`'s `C` branch releases it. A file that exists
  only in the SENDER's writable layer goes out as `T|` (content snapshot, `only_in_own_writable_layer`),
  everything else as `F|` (must stay the SAME inode on both sides); no snapshot form (a directory, >64MB)
  falls back to `F|`. Do NOT fix this class by adding `/tmp/` to `SPILLED_PREFIXES`. Repro
  `.wfgy/scmring.sh` -> `SCMRING PASS|FAIL`; `.wfgy/scmlisten.sh` (carried listener, NOT yet run) ->
  `SCMLISTEN PASS|FAIL`.
- Fixed on the way: fork children keep committed `VM_OWN_FORK_PADDING` (`067367b` -- was the renderer AV
  `error_code=0x4`, fork padding with no access bit); cross-process `/proc/<pid>` via
  `procfs::set_pid_known_fn` plus `statm`; huge `PROT_NONE` mmaps reserve address space only
  (`RESERVE_ONLY_THRESHOLD`); guest `int3` reaches the VEH; `/dev/shm`+memfd travel by named section; tar
  mtimes preserved (fontconfig was rescanned per process); `SO_PROTOCOL`/`SO_DOMAIN` (Python's
  `socket.socket(fileno=...)` -- the only way to adopt a carried fd -- probes both).
- Still open: (1) the launcher thread SERIALISES cross-process spawns, so children can still hit the 15s "no
  connection" self-termination -- native COW fork or a parallel launcher is the lever; (2) `--single-process`
  dies on a Chromium CHECK in `RenderProcessHostImpl::GetProcessHostForSiteInstance`; (3) ~1 per run
  `rebuilding a carried SCM_RIGHTS fd failed errno=ENOENT spec=F|.../Local Storage/leveldb/LOG` (the file is
  gone, not a layer-visibility miss); (4) `Vmem::duplicate` still skips `VM_OWN_FORK_PADDING` (the
  SAME-process fork); (5) GWP-ASan `MapRegion` EEXIST occasionally (`--disable-features=GwpAsan*`).
- **UNRESOLVED CONTRADICTION**: `9014df7` reported "chromium headless prints example.com" -- a page WAS
  produced once, after unix `EPOLLOUT` stopped firing when the blocked message does not fit. Diff
  `9014df7..HEAD` for unix/socket behaviour before assuming anything about the Mojo path.
