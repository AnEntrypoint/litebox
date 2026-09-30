# litebox -- current state (2026-09-29, 118th pass, later half)

CURRENT-STATE picture: what works, what is broken, what to do next. Every claim carries a commit sha
or `file:line` so the next session re-verifies instead of re-deriving; a claim nobody can point at,
or one a later commit superseded, is deleted. Reference detail lives in `docs/AGENTS_ARCHIVE_*.md`
(read for a trail, never as a starting point). This file is also the single source of truth for
standing rules: a future "remember this" is one line plus its pointer here. The previous full text
(4th-118th passes, every measurement) is `docs/AGENTS_ARCHIVE_2026-09-29.md`.

## Where things stand

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

1. **Host memory is the binding constraint, not a litebox logic bug.** 24 host processes = ~4.5GB
   working set on a 15GB box; with Chrome/Defender/etc. free RAM falls to 300-500MB, pages/sec hits
   5000, and then input is dropped or delayed (`selkies: Input X connection was unresponsive`, `Client
   stall: no ACK`, bash `sleep 30` taking minutes) and the page can die (`ERR_EMPTY_RESPONSE`). Every
   cross-process fork child starts at 74MB private / 50MB resident BEFORE guest code (172 heap regions of
   ~8MB: per-process rootfs index + tables; `LITEBOX_DIAG_MEM_BREAKDOWN=1` prints it); selkies ~1.2GB,
   Xvfb ~600MB. Biggest lever: make the merged rootfs index shared/mmap-able instead of a private heap
   copy per process; then fewer/lighter processes. `MAX_TIMEOUT` of a fork child's `net_worker` was raised
   1ms -> 25ms (25 processes polling the one cross-process network lock at 1kHz was a lock convoy);
   root stays 1ms. Re-verify the effect on a quiet host.
1b. Chrome (the user's own, ~6GB) and other host apps leave 0.3-2GB free, which makes full-stack runs die on
   the driver's `KILL low memory` guard (`avail<120`) before `DE_UP`; check `Get-Counter '\Memory\Available MBytes'`
   first. The gate in `pass118_full_err.ps1` is 1000MB.
2. Thunar: launching `thunar &` from the terminal printed `The connection is closed` then `Terminated`
   (D-Bus client to the already-running session Thunar); not root-caused. Run it alone and read stderr.
3. `at-spi` still warns `GetRegisteredEvents ... unknown signature` in GTK apps (registryd itself now
   stays up after `0cda0ec`); check whether the reply signature is truncated on the a11y bus.
4. `/proc/self/fd/` lists as empty (`ls`), only `readlink` of one entry works.
5. Selkies: one client per instance, no slot reclaim on reload; the playButton/videoCanvas gate
   (archive) no longer blocks (video shows after `Control+Alt+t`-driven session start).
6. Native kernel-COW fork on Windows (`.gm/prd.yml` `native-kernel-cow-fork`), writable layer shared
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
- **Logs**: default `warn,...fork_verify=error`; prefer the dedicated low-overhead targets over blanket module
  debug (`syscalls::file=debug` floods 50MB/s). A boot whose log stops is usually a dead root runner (a
  cross-process child has the bare 77-char command line).
- **Run dbus-daemon non-forking** (`dbus-launch` daemonizing breaks connects); for XFCE use `xfce4-session`.
  `.wfgy/webtop_stack.sh` is embedded in `webtop_seed.tar` (re-tar after edits). Readbacks inside a boot
  script use `$( )`/pipes, not `cmd > /tmp/f` + a sibling's read (writable-layer visibility gap).
- **Repo hygiene**: layer tars, frame dumps, debug logs never in git (`.wfgy/` is ignored); no test files;
  commit as lanmower only.

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
