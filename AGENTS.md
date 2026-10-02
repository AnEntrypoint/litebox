# litebox -- current state (2026-09-29, 118th pass, later half)

CURRENT-STATE picture: what works, what is broken, what to do next. Every claim carries a commit sha
or `file:line` so the next session re-verifies instead of re-deriving; a claim nobody can point at,
or one a later commit superseded, is deleted. Reference detail lives in `docs/AGENTS_ARCHIVE_*.md`
(read for a trail, never as a starting point). This file is also the single source of truth for
standing rules: a future "remember this" is one line plus its pointer here. The previous full text
(4th-118th passes, every measurement) is `docs/AGENTS_ARCHIVE_2026-09-29.md`.

## Where things stand

**2026-09-30 verification (`.wfgy/pass118_fin11.*`, HEAD ccdcd2b+)**: `DE_UP` 90s (host busy), then in real Chrome: xfce4-terminal
(pipelines, `su`, DNS, `apt-get update` 29MB + `apt-get install figlet` all work), Thunar (`thunar &` opens `/`), Mousepad,
`ls /proc/self/fd`; 12+ min soak with 30s-interval HTTP probes all 200, zero `resetting Network`, 24 host processes,
3.3GB total working set (was ~4.5GB), witness `.gm/witness/stack118_thunar_mousepad.png`. Typing into the page still
drops characters when the host is starved (keep strings short); the chrome-devtools `click` tool cannot hit injected
overlay buttons reliably now -- use `Alt+Tab`/keyboard to focus windows.

**A real XFCE desktop runs in a real browser and is usable**: `Xvfb` + `xfce4-session` (5 clients) +
`selkies` (x264, MIT-SHM) inside litebox, host reverse proxy `--publish 8081:8081`. Verified through
real Chrome CDP mouse/keyboard input (screenshots `.gm/witness/stack118_t*.png`): Applications menu,
xfce4-terminal with a working bash (prompt, echo, `tty`, job control, colours), Mousepad (typing),
the Settings manager (live search, tooltips), Thunar preferences dialogs. `DE_UP` in 25-40s.

Fixed this pass (all committed, newest first):
- `a42d9d0` **interactive-shell pipelines hung forever** (`echo hi | cat`, `apt-get update | tail`): bash's job-control
  sync pipe has both pipeline children holding its read end, and `ForkPipeBridge::Source` waited for its SIBLING bridges
  to release the end (each counting the others as owners) = mutual deadlock. A Source now delivers EOF at once when the
  parent pipe has no writer and nothing buffered (`DetachedPipeEnd::at_eof`, steals no bytes). Cheap repro:
  `script -qec "/bin/bash -i -c 'echo hi | cat'" /dev/null` in the webtop image. Also: waiters in `RawMutex::block`
  re-check the lock word each liveness tick (lost-wakeup safety net).
- `b33d6cc`/`db56a84` (subagent): `apt-get update`/`install` work (cross-process `/var/lib/apt` file spill run as root,
  pipe-bridge newest-only draining, unlinks reach the parent via whiteouts, `link()` copy-up, `getcwd`, fork children
  inherit cwd/ids/groups). `%TEMP%\litebox-spill-*` directories are never cleaned up.
- **Network lock orphaned by process exit/kill**: a process that died (ExitProcess, fatal signal, cross-process
  `kill -9` = TerminateProcess) while its `net_worker` held net_lock made recovery run `Network::reset_after_poisoning`,
  which WIPES all sockets incl. selkies' listener (port 8081 empty reply, desktop dead; `apt-get` killing `sqv`/`store`).
  Fix: `run_network_worker_round`/`quiesce_network_worker`/`exit_process_quiesced` (platform lib.rs) gate every
  net_worker round; both runner exits quiesce first; cross-process SIGKILL posts the pending bit, the target's signal
  listener calls `exit_host_process_quiesced`, the sender waits 1s (`wait_for_host_process_exit`) then falls back to
  `terminate_host_process`. Repro: python3 http.server + 250 sequential `curl` in the webtop image with
  `LITEBOX_CHILD_NET_POLL_MS=1`: old binary 2/2 runs wiped (bad=164/243), new 0/3. Residual: a guest thread inside a
  socket syscall (or a kill before the target's listener exists) still orphans; one wipe in ~5 default-cadence runs.
- `f594693` **guest identity and DNS**: `setuid`/`setgid`/`setresuid`/`setresgid`/`setreuid`/`setregid`/
  `setfsuid`/`setfsgid`/`setgroups`/`getgroups` were fixed-credential stubs that refused every change with
  EPERM (apt's `_apt` sandbox, `su`, `sudo`, `setpriv` all died); `Credentials` is now interior-mutable
  (real/effective/saved ids, groups, keep-caps retention; root changes freely, non-root only to ids it holds;
  a `fork()` child gets `fork_copy`, threads share). Cross-process fork children still restart as root
  (`lib.rs` builds them from the env ids). `capset` is accepted (capabilities are not modelled), `PR_SET/GET_
  KEEPCAPS`, `SECUREBITS`, `CAP_AMBIENT` accepted. **`IP_RECVERR` setsockopt was unsupported, and glibc's
  resolver treats that as fatal: `getaddrinfo` never worked** (raw UDP DNS did) -- found by tracing `getent`.
  Remaining apt/su problems: see Open.
- `2b7d7df` **TCP teardown**: smoltcp keeps a socket whose peer sent FIN "open" (CloseWait), so `recv()`
  never returned 0 and every guest connection the peer closed first hung its reader (Python `urlopen` of a
  guest `http.server` timed out; sockets leaked until the 256-slot `MAX_SOCKETS` table filled = the
  "selkies HTTP freezes after ~130 connections" symptom). `drain_socket_channel_buffers` now calls
  `mark_peer_closed()` (data first, then EOF; half-close writes still work); `shutdown(SHUT_WR)` sends its FIN
  only after queued data left (was dropped). TCP `accept/connect/send/recv` also wait with the bounded
  re-poll (`wait_on_events_polling`) since the owner process advances socket state. Repro:
  `.wfgy/leak2.sh` (four teardown cases) and `.wfgy/leak.sh` (HTTP loop; TIME_WAIT limits a burst to ~170).
- `88f632f` layered fs: writing `/dev/null` (any device) via a cached read-only lower fd tried to migrate the
  device up -> `EISDIR`; bash opens `/dev/null` read-only for a background job's stdin and then again for the
  redirect, so EVERY `cmd > /dev/null &` failed (daemons crashed, e.g. `python3 -m http.server > /dev/null 2>&1 &`).
- `f0ecbb0` `getsockname()` of a bound/listening TCP socket returned `0.0.0.0:0` (smoltcp has no
  `local_endpoint` until connected); now the bound address/port. `290d4d4` `mprotect` rounds length up to a
  page (`file` failed). `3ee1ce7`/`bff1d0b`: lazy-map table lock hardened (thread-id reuse, re-entry from the
  fault handler, uncommitted pages) -- a hang inside it stalled `at-spi2-registryd` in `mmap`, and every GTK
  client then waited 25-30s on AT-SPI (`dbus-daemon: Failed to activate ... org.a11y.atspi.Registry: timed
  out`), which made session start slow and flaky.
- `bff1d0b` `RawMutex` dead-holder recovery CASes from the lock word's CURRENT value and forgets the
  holder only if it really released (was: stale `val` CAS failed, holder cleared anyway = permanent orphan).
- `0cda0ec` **lazy file map**: fill a lazy range before a partial remap cuts it (ld.so maps a whole lib
  then `MAP_FIXED`-remaps segments over it; the remnants read back a garbage `.gnu.hash`, so
  `at-spi2-registryd` died `Inconsistency detected by ld.so: dl-setup_hash.c` = exit 127 on every D-Bus
  activation). Found with `litebox_diag::stderr_capture=debug`. Same commit: `epoll` bounded 15ms
  re-poll for TCP socket interests (a readiness change made by another process's network poll only
  notifies local observers -- the publish proxy lives in the root process).
- `bb518ca` per-fork inheritance env vars (`LITEBOX_INTERNAL_FORK_CHILD_{PIPE,FILE,EVENTFD,SHIM}_FDS`)
  are consumed by `take_fork_env` after use: grandchildren inherited them and rebuilt fd 0 over the
  parent's stale stdin pipe AFTER reopening the pty slave (no tty for any foreground command from a
  shell). Also: `readlink /proc/self/fd/N` reports the recorded path for fds 0-2; fstat of a pty slave
  equals `stat("/dev/pts/N")`, so `ttyname()`/`tty` work. **Any new per-child env var needs a remove
  after adoption** (same class: `ff10543`, the lazy descriptor path).
- `d383f90` pty: `O_NONBLOCK`/`O_NDELAY` given at `open("/dev/ptmx")` is honoured (VTE's blocking master
  read froze the terminal); `TIOCPKT` packet mode prefixes every master read with status byte 0 (VTE
  dropped all output because the first byte was read as a control packet).
- `d1dae93` pty master carried across cross-process fork (`SharedPtyTable` holder counts, `pty-master:`
  shim-fd spec): GLib/VTE child setup uses the inherited master before exec.
- `a61ed74` lazy-map table lock (`DeadHolderLock`) recovers from a dead owner thread: `exit_group`
  terminates sibling threads, one died inside the VEH holding the std Mutex, the exiting process hung in
  `forget()` and the parent's `wait4` (selkies startup) never returned. 10/10 selkies starts.
- `820c2d6` external fault watchdog inert until the guest starts; `28739ab` resumable OCI layer
  download; `be7be6d` pin mode `LITEBOX_OCI_USE_LAST_RESOLVED=1` (re-pointed tag cannot trigger a
  multi-GB re-pull); `5de3881` `Pipes` holds no process-relative state; `d90ad7d` every ELF in a layer
  is pre-patched (not only `0o111`); `8760e2c`/`5a07225` `LITEBOX_LAZY_FILE_MAP=1` (resident memory
  follows touched pages; fork children re-arm from a descriptor file).

## Open, in rough priority order

1. **Host memory is the binding constraint, not a litebox logic bug.** Fixed (b11f674, 6fd27b7, d07a7f9): a fork child was 72MB committed / 41MB resident
   before guest code (slim; 61/58MB on webtop), now 34/8MB (webtop 61/26MB): the regex-based `EnvFilter` and the child's `Reference::parse`+tokio+client
   compiled a 30MB Unicode regex NFA into the never-shrinking buddy heap (`pull_layers_with_known_digests` now returns straight from the layer cache);
   `copy_one_group` wrote the parent's 8MB all-zero guest stack into every child (zero pages skipped); frees >=256KB now `DiscardVirtualMemory` their pages
   (`MemoryProvider::release_pages`); a partial `mprotect`/remap of a lazy file mapping filled the WHOLE library (libLLVM text = 117MB resident in each
   dlopen'er), now only edge chunks. Full stack at t=150s: private resident 2246MB -> 1222MB over 20 processes. Diagnose with
   `LITEBOX_DIAG_ALLOC_STACK=1` (stack RVAs of every >=1MB host allocation; symbolize with `llvm-symbolizer`) and `LITEBOX_DIAG_MEM_BREAKDOWN=1`.
   Round 2 (f271ed2 + idle trim): the rootfs index is a flat pointer-free image (`TarRo::flat_index`/`from_flat_index`, cache file `mergedidx_*_v3`) that
   children mmap read-only (webtop child 26MB -> 6MB resident); the OCI reference is parsed without the Unicode regex (`parse_reference`, root 76 -> 40MB);
   `idle_trim.rs` empties the working set of a host process that used <6% CPU over 4s (`LITEBOX_IDLE_TRIM=0` disables), so idle daemons' pages go to
   standby: full stack WSsum 2.8GB -> ~0.4GB with the desktop in use (private WS 1.5GB -> 0.3GB). `.wfgy/memsamp.ps1` samples WS/private WS/avail;
   `.wfgy/wsmap.ps1 -ProcId` splits one process's resident pages by region type. Remaining: selkies/Xvfb heaps (150-250MB private RW, legitimate guest
   memory), guest `PROT_NONE` reservations are committed (commit charge only), an idle process still burns 1-3% CPU (poll threads).
   Beware: a heap layout that differs between root and child exposes latent shared-struct host pointers (`bootstrap_process` was one; 6fd27b7).
1b. Chrome (the user's own, ~6GB) and other host apps leave 0.3-2GB free, which makes full-stack runs die on
   the driver's `KILL low memory` guard (`avail<120`) before `DE_UP`; check `Get-Counter '\Memory\Available MBytes'`
   first. The gate in `pass118_full_err.ps1` is 1000MB.
2. Fixed 119th pass (verify in the full stack, not yet seen in a browser): `3e1ef47` SCM_RIGHTS over a
   cross-process unix connection (thunar's D-Bus call passes a dup of stdin; the refusal made GDBus close the bus =
   `The connection is closed` + SIGTERM). Only regular files, pty slaves, stdio, eventfds cross (spec text in
   `RingFdMail`, rebuilt by name); pipes/sockets/pty masters still get EOPNOTSUPP. `b012910` lazy file map filled
   stale entries over an execve'd fork child's libraries (registryd died in ld.so, source of the at-spi
   `unknown signature` warning). `08ae94f` `/proc/<pid>/fd` lists (own pid only, snapshot).
   Repro scripts: `.wfgy/th1.ps1 -Run th1|th2|th3` (+ `th*.sh`; env `TL`=timeline comms, `LZ`=lazy 0/1, `LZD`).
3. Selkies: one client per instance, no slot reclaim on reload; the playButton/videoCanvas gate
   (archive) no longer blocks (video shows after `Control+Alt+t`-driven session start).
4. Native kernel-COW fork on Windows (`.gm/prd.yml` `native-kernel-cow-fork`), writable layer shared
   across processes (`shared-writable-layer-across-processes`), AF_UNIX exhaustion silent
   (`SharedUnixAddrPresenceTable` 256 slots, keys >108 bytes), `flock_registry`/`drm`/`evdev` per-process,
   `timerfd`/`signalfd` uncarriable, fixed-address (non-PIE) exec from a same-process vfork child collides
   (gcc): needs cross-process spawn with fd carrying. macOS/Linux builds cannot be verified on this host.

## How to run and drive it (harness lessons that cost sessions)

- Cheap repro: `target/release/litebox_runner_linux_on_windows_userland.exe -Z --oci-image docker.io/
  library/debian:stable-slim -- /bin/bash -c '<script>'` (or the `webtop:debian-xfce` image, cached).
  **PowerShell, never Git Bash** for anything with guest paths. Single quotes only inside `-c`. Redirect
  with `cmd /c "... < script.sh > out 2> err"` (Start-Process redirection kills the runner; `*>` logs are
  UTF-16LE and word-wrap at ~116 chars). `LITEBOX_PROCESS_FORK=1` is a HOST env var. Confirm the release
  exe mtime postdates the newest commit; `cargo build` cannot replace a running exe (kill runners first).
- Full stack: `.wfgy/pass118_full.ps1 -Run <name> -MaxSeconds N` (+`.sh`). It waits for 2.5GB free RAM
  before starting (the log file appears late) and **terminates every `litebox_runner` at its cap -- a stale
  older driver kills a newer run** (cost two sessions: "session-client death cascade", "selkies rc=137").
  Kill leftover driver powershells before each run; give a cap longer than you need. Variants:
  `pass118_full_err.ps1` (adds `litebox_diag::stderr_capture=debug`), `pass118_full_a11y.ps1`.
- Browser: `mcp__chrome-devtools__*` for real CDP input. `click` needs a uid, so inject a fixed,
  `pointer-events:none`, opacity .01 `<button id=probe>` at the wanted x,y and click that (the real mouse
  event lands on the video). `Control+Alt+t` opens Terminal (allow 30s). Type slowly/short strings; a
  burst is dropped when the host is starved (see Open 1). gm `cdp` is JS-eval only.
- Diagnostics that paid off: `cdb -pv -p <pid> -xd av -xd sse -c "~*kb 25; qd"` on every runner, symbolized
  with `llvm-symbolizer --obj=<exe> --relative-address` (`.wfgy/symstk.py`; release ICF mislabels callers,
  build without `--release` when the exact caller matters); `LITEBOX_DIAG_SYSCALL_TIMELINE=<comm,...>`;
  `litebox_diag::{process_timeline,socket_read,unix_conn_teardown,stderr_capture}=debug`;
  `LITEBOX_DIAG_NO_EXTERNAL_FAULT_WATCHDOG=1`/`LITEBOX_DIAG_NO_FAULT_WATCHDOG=1` before any `cdb` attach
  (the driver sets them); `xproc.rs` logs `xproc SIGKILL` (target/host pid) when a guest kill terminates a
  host process. A cross-process child whose exit code lacks the `0xC0DE` marker decodes as SIGKILL
  (`decode_cross_process_exit_status`), so `rc=137` also means "host process died some other way".
- **Before blaming litebox for a death or stall, correlate it with the harness's own time caps, kill loops
  and the host's free RAM / pages-per-sec first.**

## Standing lessons and hard constraints

- **No WSL/hypervisor ever.** Never `bcdedit /debug on` without a kernel debugger. A spinning process that
  resists `Stop-Process` needs WMI `Terminate`. `cdb` must use `-pv`/`qd`, never bare `q`.
- **Never run two full-stack verifications concurrently**; watch `Available MBytes`, kill on a falling trend.
- **Guest-reachable code returns an errno, never a panic** (the host process IS the whole guest session).
  Refusal errno is API contract: EPERM lets callers degrade, EINVAL/ENOSYS fails them hard.
- **`LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check.** Never time litebox with one host
  process per datapoint (spawn costs 1.6-2.3s). Never trust a container tag name for its contents.
- **A `TypedFd` index is valid only against the `Descriptors` that inserted it**; shared-memory structs must
  hold no process-relative pointers (`Network`, `Pipes`, `FutexManager` were all this bug). A mutable table
  on the `SharedUnixAddrPresenceTable` pattern needs every write path mirrored to the shared side.
- **Cross-process fork (`LITEBOX_PROCESS_FORK=1`)**: a genuine `D==0` fork (`spawn_cross_process_fork_child`);
  pipes/regular files/eventfds/pty slaves+masters/unix sockets are carried, cloexec+pty fds are dropped, only
  unrecoverable kinds refuse. `live_cross_process_fork_children` caps concurrent children at 6. Proving a run
  took this path needs `[process_fork_diag] task-resume-probe` lines. Cross-process `kill()` goes through
  `GlobalState::process_table` (`syscalls/signal/xproc.rs`). `fork_verify.rs` healing is Windows-only.
  The env var is **presence-checked** (`std::env::var_os("LITEBOX_PROCESS_FORK")?`, `litebox_shim_linux/src/lib.rs:13424`):
  `LITEBOX_PROCESS_FORK=0` still ENABLES it -- unset the variable to get the same-process path.
- **`chroot(2)` is real (e608959)**: `FsState.root` (next to `cwd`) is shared by `CLONE_FS`, so a chroot on any
  task sharing that state -- a pthread, a `CLONE_FS` clone -- roots every one of them, parent included; `cwd`
  is stored in root-space, dirfd-relative paths stay unrooted (an fd opened before the chroot still escapes).
  Do NOT test `CLONE_FS` with a raw `clone(CLONE_VM|CLONE_VFORK|CLONE_FS)` from CPython: the child runs but the
  parent never resumes and the process dies 139 (reproduced with a child that only `_exit`s, no chroot at all);
  a plain `clone(CLONE_FS|SIGCHLD)` either goes cross-process (nothing can propagate) or segfaults on the
  same-process eager-duplicate path. Use pthreads, which pass `CLONE_FS`.
- **Logs**: default `warn,...fork_verify=error`; prefer the dedicated low-overhead targets over blanket module
  debug (`syscalls::file=debug` floods 50MB/s). A boot whose log stops is usually a dead root runner (a
  cross-process child has the bare 77-char command line). **Log verbosity comes from `LITEBOX_LOG`,
  not `RUST_LOG`.** Disk hygiene: a Chromium run with `--enable-logging=stderr --v=1` produced a
  1GB log in 2.5 minutes from one duplicated warn -- sample repeated messages (a stride counter) or
  cap them (see `MAX_AV_PATH_HEALS`, `AV_HEAL_LOG_SAMPLE_STRIDE`) before re-running.
- Cheap guest repro without a file in the guest: `.wfgy/guest2.ps1 -Script <sh> -Run <name> -Secs <n>`
  (base64's the script onto the command line; sets LITEBOX_PROCESS_FORK=1, LITEBOX_LAZY_FILE_MAP=1,
  LITEBOX_OCI_USE_LAST_RESOLVED=1; `-ExtraEnv "K=V;K2=V2"` overrides). Build only
  `cargo build --release -p litebox_runner_linux_on_windows_userland` (~1m); the whole workspace
  does not build on Windows.
- **Run dbus-daemon non-forking** (`dbus-launch` daemonizing breaks connects); for XFCE use `xfce4-session`.
  `.wfgy/webtop_stack.sh` is embedded in `webtop_seed.tar` (re-tar after edits). Readbacks inside a boot
  script use `$( )`/pipes, not `cmd > /tmp/f` + a sibling's read (writable-layer visibility gap).
- **Repo hygiene**: layer tars, frame dumps, debug logs never in git (`.wfgy/` is ignored); no test files;
  commit as lanmower only.

## Linux runner (`litebox_runner_linux_userland`, merged from branch `claude/modest-feynman-3zpzop`)

Cloud sessions are Linux; there the `webtop:debian-xfce` rootfs (`--initial-files rootfs.tar --rewrite-syscalls --uid 0 --gid 0
--pid1 --tun-device-name tun0 ... /bin/sh /init`) boots under the Linux runner: s6, Xvfb, xfwm4/panel/xfdesktop, nginx, pulseaudio,
dbus, Selkies; a host browser at `http://10.0.0.2:3000` (TUN, host 10.0.0.1) drives it (xterm, xfce4-terminal, Thunar, Mousepad,
Settings, Chromium verified). Harness: `tools/webtop/`. Not re-verified after the merge with the Windows line.
- Native fork (`has_native_fork()==true`, host `fork()`): shared-arena fork with pool-backed shared memory; wait/notify hooks are
  `waitpid`/`waitid(WNOWAIT)`; `SYS_wait4`/`SYS_waitid` seccomp-allowed (a blocked host syscall answers EINVAL and looks like a hang);
  `exit_native_fork_child` ends the child with the guest status. A native-fork child COW-copies every private kernel structure, so
  shared state must live in the shared arena. The thread-based fork corrupts guest memory on Linux -- not a substitute.
- Per-process `/proc/self` identity, `/proc/<pid>/{task,oom_score_adj,environ,fd}`; real pty line discipline; SCM_CREDENTIALS;
  shared-mapping coherence; per-thread fs "act as root" guard; network worker never blocks on a per-descriptor lock
  (`iter_nowait`); runner panic = `_exit(134)`; big read-only private file maps are ONE shared object; freed heap >= 1MiB returns
  pages (`MADV_REMOVE`). Desktop plateaus near 9GB (cgroup limit ~14GB).
- Added: inotify, `MSG_PEEK` on unix+inet (used to consume bytes; broke TLS), xattr stubs, timestamps (tar mtimes), `mincore`,
  `copy_file_range`, `splice`, `rt_sigtimedwait`, `rt_(tg)sigqueueinfo`, `O_TMPFILE`, NETLINK_ROUTE link/addr/route dumps,
  `pidfd_open`/`waitid(P_PIDFD)` (also reaps cross-process children), `getrusage`/`mlock`, ptrace answers EPERM.
- Dead-holder recovery for `RwLock` (write owner thread token) and platform `RawMutex`: a waiter blocked 2s checks `tkill(tid,0)`
  (`litebox::fs::ident::set_thread_id_fn`/`set_thread_alive_fn`); up to 4 readers tracked. A panic holding the layered-fs root write
  lock hangs every later `open`.
- Debug kit: `LITEBOX_DIAG_FAULT=1 LITEBOX_PRINT_EXE_BASE=1` + frame-pointer build (`[diag-fault]`, `[diag-hostpid]`, `addr2line`);
  `LITEBOX_DIAG_BIGALLOC=1`; stalled boot: `gdb -p <root> -batch -ex "thread apply all bt"`. Unit tests in litebox_shim_linux:
  `RUST_MIN_STACK=64M`, `--skip tun`. Known gaps: IPv6 rides the IPv4 machinery; `/proc/<pid>/fd` shows non-path fds as `anon_inode:[N]`.
## Cross-process shared memory and locks (all DONE; mechanism in the archive)

`RawMutex` is a manual wait queue + cross-process `Event`s with `holder_pid` dead-holder recovery
(`bff1d0b`). A 64MiB `shared_kernel_arena_alloc` backs `SharedArc<T>` (`GlobalState`, `Network`, pty/unix
tables); `SLAB_ALLOC` stays per-process. `SharedArc::new` shares only `T`'s inline bytes -- a `BTreeMap` has
private-heap nodes, so shared registries are fixed-slot, lock-free, atomic tables. Still per-process:
`flock_registry`, `SafeZoneAllocator`'s `SpinMutex` (no dead-holder recovery, theoretical).

## Cross-process file visibility, identity, apt (2026-09-30)

- A file written by one host process is invisible to siblings until it exits (per-process writable layer).
  `syscalls/file_spill.rs` write-through-spills paths under `SPILLED_PREFIXES` (`/var/lib/apt/`,
  `/var/cache/apt/`) to `%TEMP%\litebox-spill-<rootpid>\<slot>.bin` (platform `spill_*`, index in
  `GlobalState.shared_file_spill`, per-slot generation); open/stat/access/getdents/unlink/rename refresh the
  local copy. Fixes `apt-get update` (http method writes InRelease, sqv/apt-get read it). Extend the prefix list,
  not the mechanism, for the next such directory.
- Direction matters: a cross-process fork child receives the parent's exported writable layer AT SPAWN (its
  `[process_fork_diag] task-resume-probe (child): guest fd N reopened on /tmp/...` lines are that layer being
  readable), while a child's own writes reach the parent only when it exits and `wait4` imports them
  (`litebox-forkwrite-*.tar`). So a fork carry of a fresh `/tmp` fd works, and the same fd handed back the
  other way over SCM_RIGHTS does not. `only_in_own_writable_layer` (see the Chromium section) is how the carry
  path tells those apart.
- The fs layers check permissions against the calling task's fsuid/fsgid (`litebox::fs::set_effective_identity`,
  set at every syscall entry); root bypasses rwx bits. `chown` is real, export/import carry owner and full mode.
  Anything that walks the fs outside a syscall (export/import) must run inside `litebox::fs::with_root_identity`.
- The external fault watchdog only counts stalls after the VEH sets the `litebox-fault-armed-<pid>` event; before,
  it killed any idle method process (apt's sqv, exit code 1 = "signal 9").
- `NETLINK_AUDIT` sockets ack every `NLM_F_ACK` message (libaudit/PAM need the ack); `getpriority`/`setpriority`
  exist (pam_limits aborts on ENOSYS); AF_INET6 sockets answer `getsockname` etc. as v4-mapped `sockaddr_in6`.
- Cross-process fork children now inherit cwd and full credentials (`task-state:` shim spec, fd `i32::MAX`), a child's deletions reach the parent as `.wh.` tar entries, and only the newest Source pipe bridge of a read end drains (older siblings deadlocked dpkg-deb). `apt-get install -y file` completes.

## Containers and OCI

`litebox_packager --oci-image <ref> --output <tar>` pulls, merges, rewrites every ELF; the runner does the
same in memory (`.litebox-cache/`, keyed by `REWRITER_CACHE_VERSION`; `LITEBOX_OCI_USE_LAST_RESOLVED=1` pins).
Verified: `linuxserver/webtop:debian-xfce`/`ubuntu-xfce` ship XFCE, `alpine-mate` ships MATE, `alpine-xfce`
does not exist. For `--gui` DRM use `Xorg` with `modesetting`; for browser/selkies `Xvfb` is right.

## Closed -- do not re-attempt without a genuinely new approach

VEH_FRAME_STRIDE canary, `dev_bench`/`litebox_runner_snp` build failures, CoW-mmap performance, input
latency (pre-118th), presenter-split duplicate `SYN_REPORT`, GUI-protocol decision, `spawn_suspended`
stdio-handle bug, presenter-process split, ACK-stall-kill and port-8081 watchdog (2026-09-16),
"session-client death cascade" (was the driver cap), `xfdesktop`-first client deaths (same).

## Docs and tooling map

- Archives, newest first, all under `docs/`: `AGENTS_ARCHIVE_2026-09-29.md` (full 118th-pass text),
  `_2026-09-28.md` (4th-116th narrative), `_2026-09-23.md`, `_2026-09-22.md`, `_2026-09-18.md`,
  `_2026-09-17.md`, `_2026-09-16.md`, `_2026-09-15.md`, `_2026-09-10.md`, older `_2026-09-03/05.md`.
- Fork: `docs/track-b-fork-fix-progress.md`, `advisor/ADVISORY-002-d-zero-fork.md`,
  `advisor/ADVISORY-001-fundamentals.md`; VEH: `docs/veh-exception-handler-design.md`.
- `docs/diag-timeline-field-semantics.md`, `docs/premade-library-research.md`,
  `docs/drm-dumb-buffer-ioctl-reference.md`; `docs/macos.md` (stub); `advisor/probes/` (decode_frame.py,
  symbolize_litebox_crash.py, MEASUREMENT-PITFALLS.md, DISK-HYGIENE.md); `.gm/memories/` (superseded).

## Chromium (2026-09-30) -- runs multi-process, does not yet finish a page

`chromium --headless --no-sandbox --dump-dom` (webtop image, `LITEBOX_PROCESS_FORK=1`) now starts the browser,
network/storage/renderer children over Mojo, and reaches the network; no page is produced yet. Fixed: huge
`PROT_NONE` mmaps reserve address space only (`RESERVE_ONLY_THRESHOLD`, commit at mprotect, ENOMEM not panic);
`Sysinfo` `repr(C)`; guest `int3` reaches the VEH (`EXCEPTION_BREAKPOINT` in the asm whitelist); `getrlimit`
EFAULT; cross-process fork: `/dev/shm`+memfd travel by named section (`MemfdEntry::name`, `S|` SCM spec, `shm:`
fork spec), epoll/netlink dropped, child inherits `/proc/self` exe/cmdline (`task-state`), `/proc/self/task`,
dir nlink 3, `stat(/proc/self/fd/N)`; unix sockets over SCM_RIGHTS (`U|`); `promote_for_fork` only folds
peer-shutdown on the FIRST promotion; tar dir/file mtimes preserved (fontconfig caches were rescanned every
process; `MLE3`); a fork child's adopted claims no longer look foreign (`mark_fork_child_host`).
Open, in order: (1) every cross-process fork costs 3-5s in the parent (`spawn_cross_process_fork_child`
warn line; pristine file-backed chunks are skipped, the rest of the 328MB image group still copies) and the
launcher thread serialises them, so children hit the 15s "no connection" self-termination -- native COW fork
or lazier copy is the lever; (2) `--single-process` dies on a Chromium CHECK in
`RenderProcessHostImpl::GetProcessHostForSiteInstance`; (3) GWP-ASan `MapRegion` EEXIST still shows up
occasionally (`--disable-features=GwpAsan*` avoids); (4) SCM_RIGHTS of file fds whose path no longer resolves;
(5) the desktop-typed `chromium --no-sandbox &` was not verified (typing dropped on the starved host).
Repro scripts: `.wfgy/chromium_headless.ps1 -Run <n> -Secs N -Extra "<flags>"`.
- `seccomp(2)`/`prctl(PR_SET_SECCOMP)` are real now: a classic-BPF interpreter
  (`litebox_shim_linux/src/syscalls/seccomp.rs`) evaluated on the raw syscall number at the top of
  `Task::do_syscall`, before `SyscallRequest::try_from_raw` -- ALLOW/LOG run it, ERRNO returns it,
  TRAP/KILL/TRACE deliver SIGSYS via the existing fatal-signal path. Mode, `no_new_privs` and the
  filter stack live on `Process` (TSYNC is free, all three survive clone and execve); a cross-process
  fork child rebuilds the parent's mode, `no_new_privs` and filter programs from the `task-state`
  spec (`SeccompState::restore_from_spec`, so it is NO LONGER unfiltered), and `/proc/self/status`
  gained `Seccomp:`/`NoNewPrivs:`. A filter
  that traps/kills logs `seccomp: SECCOMP_RET_*` with the syscall name (errno verdicts are not
  logged -- Chromium takes hundreds of those per second on purpose).
  Sandbox-enabled repro: `.wfgy/chromium_sandbox.ps1 -Run <n> -Secs N -Extra "<flags>"` (no `--no-sandbox`).
- Chromium must run as a NON-ROOT uid (`setpriv --reuid 911 --regid 911 --init-groups`) or the
  browser refuses to start its sandbox, and `--user-data-dir` must be `chmod 777` when a root shell
  created it (otherwise `Failed to create .../SingletonLock: Permission denied` = exit 21). Current
  repro: `.wfgy/chr12.sh` via `.wfgy/guest2.ps1 -Script .wfgy/chr12.sh -Run <n> -Secs N` (add
  `-ExtraEnv "LITEBOX_LAZY_FILE_MAP=0"` to test without the lazy file map).
- With seccomp real, Chromium's own sandbox ACTIVATES (`Activated seccomp-bpf sandbox for process
  type: renderer/utility`), no "No usable sandbox!" FATAL. The renderer then dies ~0.15s in: every
  renderer (guest pids 57/66/72) faults identically, `Exception(14) error_code=0x4`, `rip` inside
  the chromium binary, `cr2=0x2c15064` in `0x1e60000..0x3e50000`, whose VMA is
  `VM_MAYREAD|VM_MAYWRITE|VM_MAYEXEC|VM_OWN_FORK_PADDING` -- a fork-padding placeholder with no
  access bit, i.e. that range got no real mapping in the fork child. `.wfgy/mres2.sh` (mmap
  PROT_NONE, mprotect a subrange, fork, child reads/writes/mprotects) passes, so the plain
  reservation-inheritance path is not it. The log also shows Chromium's own
  `Unexpected SIGSYS received.` -- check whether a filter trapped a syscall before chasing the
  memory fault.
- **That renderer fault is FIXED (uncommitted; verified by the `chr14` run, 2026-10-02).** Root
  cause: `VM_OWN_FORK_PADDING` ranges carry no access bit, so `do_clone`'s copy-group filter
  (`syscalls/process.rs`) skipped them and `Vmem::adopt` (`litebox/src/mm/linux.rs`) re-created
  them as reserve-only `PROT_NONE` -- a cross-process fork child lost committed pages its parent
  had (`error_code=0x4` = not-present, not a protection violation). Fix: admit
  `VM_OWN_FORK_PADDING` in the copy-group filter, and keep padding ranges committed in `adopt`.
  Same blind spot still open in `Vmem::duplicate` (`linux.rs` ~1901, the SAME-process fork: it
  treats padding as "PROT_NONE, nothing to copy" and cannot read the source bytes). After the fix
  `chr14` shows: sandbox activated for 1 utility + 2 renderers, no renderer AV at all, real
  network traffic (GCM registration to android.clients.google.com), Blink loading pages.
  Still open from that run: no `--dump-dom` output; `/proc/57/status`, `/proc/41/task`,
  `/proc/57/task` ENOENT; one process (pid 38) killed by our own
  `implausible guest context on resume ... rip=0` SIGSEGV path; and 14 `SECCOMP_RET_TRAP`
  SIGSYS deliveries (sched_getaffinity/newfstatat/sched_getparam/sched_getscheduler, all in
  pid 57) after which Chromium logs `Unexpected SIGSYS received.` and continues.
- **Why no page yet (`chr15`, also true with `--no-sandbox`, so NOT a sandbox problem): Mojo IPC
  cannot hand a socket to another process.** One 300s run produced 1264 `cannot cross a process
  boundary ... kind=unix-socket` + 1261 `EOPNOTSUPP` refusals and 2527 `SCM_RIGHTS` lines total
  (`syscalls/file.rs:8161` `scm_carry_spec` -> `unix.rs` refusal): Chromium sends an unnamed
  `socketpair` endpoint over a unix socket to each child and we refuse, so the Mojo channel is
  never established. `U|` carry already exists for the FORK path, so the fix is to reuse it for
  SCM_RIGHTS. Secondary, now FIXED: a carried `F|...` file fd rebuilt ENOENT when the file lived in
  the SENDER's own writable layer (`chr16`: `spec=F|1|0|/tmp/cu3/Default/Local Storage/leveldb/LOG`,
  a `--user-data-dir` fd handed to a process that never received that layer). Predicate:
  `litebox::fs::FileSystem::only_in_own_writable_layer` (default `false`; `layered::FileSystem`
  answers "in `upper` and not in `lower`", so a copied-up rootfs file and a copied-up ancestor dir
  still count as shared), reached as `Task::path_only_in_own_writable_layer` and used by
  `Task::carriable_file_spec_for_raw_fd`: those go out as `T|` (`snapshot_nameless_file_for_carry`),
  everything else keeps `F|` -- a rootfs file must stay the SAME inode on both sides. No snapshot
  form (a directory, or >64MB) falls back to `F|`: today's errno, never a panic. Do NOT fix this
  class by adding `/tmp/` to `SPILLED_PREFIXES`: it write-throughs EVERY guest temp write to
  `%TEMP%` and refreshes on every open/stat/getdents. Repro, NOT yet run: `.wfgy/scmrights_tmp.sh`
  via `.wfgy/guest2.ps1 -Script .wfgy/scmrights_tmp.sh -Run scm1`. For the repro alone, adding
  `/tmp/cu3/` to the prefix list is legitimate -- the reasoning that put `/tmp/.config/chromium`
  there. Residual: a fork child adopts the parent's layer into its own `in_mem`, so an inherited
  file also answers "mine alone" and goes out as `T|` (right bytes, but unlinked, no path).
- **SCM_RIGHTS and a cross-process fork now share ONE carry path (2026-10-02).** `scm_carry_spec`
  (`syscalls/file.rs`) used to return `Option<String>`, so "refused" and "no rebuild for this
  kind" were the same value and the reason never reached a log; it now returns
  `Result<Option<String>, &'static str>` and `net.rs` logs `reason=` with every refusal -- read
  that field in the next `chr` run's `.err` before assuming WHICH state Chromium's sockets are
  in, since this pass was written without it. `scm_carry_spec` calls `UnixSocket::fork_carry`
  with `child_pid == 0`, now documented as the SCM_RIGHTS flavour: a listener is no longer
  advertised under a pid no process has (that is what used to force the carry to be abandoned --
  it now registers nothing and the RECEIVER registers the address under its own pid, flagged by
  a trailing `,1` on the `L` spec, which a fork's 7-field spec reads as absent = "do not"), so a
  listener carries as well. A connected endpoint, an unbound socket and a listener now cross; a
  bound-but-unconnected socket, a connect in progress and a bound or connected DATAGRAM socket
  are still refused with EOPNOTSUPP on purpose (`SharedView::send` says which).
  Repro (a socketpair end and a listener, both created AFTER the fork, so only the carry can
  deliver them; the parent closes its listener copy, so its `connect()` has to reach the child's):
  `.wfgy/guest2.ps1 -Script .wfgy/scmring.sh -Run scmring1 -Secs 90` -> `SCMRING PASS|FAIL`. Also seen: `Failed to adjust
  OOM score of renderer with pid 87: No such file or directory` = `/proc/<pid>/oom_score_adj` for
  a pid in another host process.
- **`/proc/<pid>` for other host processes (chr14's `/proc/57/status`, `/proc/41/task`,
  `/proc/57/task` ENOENT)**: `ProcSelfTable` is per host process, so a pid running in a sibling
  Windows process never had a row. `/proc` now resolves a pid directory when it has a local row,
  when it is the caller's own pid, or when `GlobalState::process_table` has it -- the last via
  `litebox::fs::procfs::set_pid_known_fn`, a plain `fn` pointer hook (set in `LinuxShimBuilder::build`) because `/proc` is mounted by `default_fs` BEFORE `GlobalState` exists and must hold
  nothing process-relative. Added `/proc/[pid]/statm` (there was no `statm` anywhere) and
  `/proc/self/statm`, rendered by a new `ProcSelfInfo::statm` closure over the same page manager
  `maps` uses (nulled in `inherit`/`portable_snapshot`); `install_task_state` now publishes a
  pid-only row plus one warn instead of silently nothing when the carried identity is absent.
- Next sandbox blocker is NOT seccomp: `zygote_host_impl_linux.cc:117` requires
  `Credentials::CanCreateProcessInNewUserNS()`, and the webtop image ships no `chrome_sandbox` SUID helper, so
  it FATALs "No usable sandbox!" (as root it dies earlier at :102, crbug 638180). Measured: `unshare` accepts
  CLONE_NEWUSER alone, EPERMs CLONE_NEWPID/NEWNS/NEWUTS/NEWIPC/NEWNET/NEWCGROUP and every combined mask;
  `/proc/self/{uid_map,gid_map,setgroups}` exist, `/proc/<pid>/uid_map` does not (ENOENT).
