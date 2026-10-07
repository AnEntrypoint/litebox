# litebox archive -- 2026-10-03 (121st pass)

Long-form detail trimmed out of `AGENTS.md` during its 2026-10-03 compaction. A trail, not a starting
point: if a claim here contradicts `AGENTS.md`, `AGENTS.md` wins.

## The chrD7 freeze investigation, in full

Symptom: a full desktop run wedged right after `DE_UP`; the whole guest froze, not just selkies.
`LITEBOX_DIAG_LOCKSTALL=1` (chrD5) split it in two:

- 16+ processes parked `val=2` on ONE arena mutex `0x7ff803061028` (>= `SHARED_KERNEL_HEAP_BASE`
  `0x7FF8_0000_0000`, `lib.rs:11370`, so genuinely cross-process) whose `holder_pid` (the process hosting
  guest pid 20 = selkies) was ALIVE -- 295 "held by a live process" warnings, ZERO recoveries. `cdb` showed
  that holder's net_worker thread inside `Network::internal_perform_platform_interaction` blocked in
  `RawRwLock::read_contended`.
- A separate spin: `val=1 now=1 holder_pid=0 queued=true pending_wake=false` (per-thread waker condvars)
  at ~1000 iterations/s -- the `chunk_ms=0` bug. Not the freeze, but it burns CPU.

Fix = `16f3e76` (best-effort `try_*` descriptor-table access in the net worker) + `5b7ff62` (guest-side
`net_lock` drop before `spin_loop`). chrD7 then ran to completion with zero lockstall warnings.

Ruled out along the way: the `close(2)` HUP/linger wait (`wait_on_events` runs `try_op` FIRST,
`polling.rs:59`; only `GracefulIfNoPendingData` can park, `net/mod.rs:1471`; `close_socket` warns when it
defers -- 0 hits); the TUN thread; panics/resets; waiter-queue overflow; dropped cross-process wakes; and
the reverse ordering (descriptor table/entry -> `net_lock`) which does not exist anywhere:
fork's fd export runs before `net_lock` is taken (`lib.rs:4173`, `process.rs:4614`), `resolve_scm_rights_fds`
drops its table guard before `scm_carry_spec` (`net.rs:2094` vs `:2171`), execve's `close_on_exec` drops
`rds` first (`file.rs:3565`), and exit teardown drops it too (`lib.rs:1946`).

## The subprocess wedge (sub1-sub5) -- selkies' blocker

Selkies' log stops at `DEBUG:ws:Starting DataStreamingServer...`; the sitecustomize spawn hook logged
`spawn ENTER ('xset','q')` and `spawn ENTER ('/usr/local/bin/selkies-proot','check')` with NEITHER ever
logging EXIT, and nothing after -- including the t=90s `faulthandler` dump, which is what a thread blocked
HOLDING THE GIL looks like. `start_server()` (`stream_server.py:3020`) only logs "running on" after
`runner.setup()`+`_start_sites()`.

Selkies spawns through:
- `_run_command` (`websockets_mode.py:4508`): `create_subprocess_exec` with `PIPE`/`PIPE`, then
  `wait_for(proc.communicate(), 10.0)`, `proc.kill()` on timeout, then `await proc.wait()`.
- `_run_detached_command` (`:4485`): `DEVNULL`/`DEVNULL` + `start_new_session=True`.
- `printing.py:241`: `create_subprocess_exec(cupsd, ...)`, with `preexec_fn` -- `_die_with_parent`'s
  `ctypes prctl(PR_SET_PDEATHSIG)` (printing.py:239) raises `Exception occurred in preexec_fn` under
  litebox, and `PrintQueue.start()` only catches `OSError`, so cupsd can never start.

Probes, in order:
- **sub1** (uvloop, six spawns with `wait_for(20)`): `/bin/echo` and `/bin/sh -c` complete in 0.17/0.18s;
  `/usr/bin/xset q` SPAWNS (pid=17) and `communicate()` never returns -- no DONE and not even `wait_for`'s
  TimeoutError.
- **sub2** (shell level, `timeout 8` wrappers, no event loop): `A_echo` rc=0, `B_xset` rc=0 with real
  output, `C_xset` without DISPLAY rc=1, `D_xprop -root` rc=0, `E` `selkies-proot check` rc=111
  (`s6-envdir: fatal: unable to envdir /run/s6/container_environment`), `F` a python AF_UNIX connect to
  `/tmp/.X11-unix/X1` OK in 0.03s (and `X99` -> FileNotFoundError in 0.00s), `G_xset_again` rc=0. So the
  CHILD is not blocked: the exec'd programs and the X connection are all fine.
- **sub3** (uvloop, split read/wait/poll + a `waitpid` probe): `echo` -> SPAWNED, `readline` got data,
  child reaped (`kill=errno=3 proc=False`), `wait()` rc=0 at 5.22s. `xset` -> SPAWNED pid=16, `readline`
  got `b'Keyboard Control:\n'` at 0.80s, POLL i=0 shows it alive, then the child exits with encoded status
  `0xc0de0000` -- and the parent prints NOTHING more for the remaining ~297s, while Xvfb lives on until the
  harness cap. `loop.add_signal_handler(SIGCHLD, ...)` raises `RuntimeError: it is used by the event loop to
  track subprocesses`, so Python cannot observe SIGCHLD itself.
- **sub4** (no waiting at all; a plain python TICK thread alongside): the TICK thread runs the whole time
  and `os.waitpid(15, WNOHANG)` returns `(15, 0)` at t=1.1s then `errno=10` (ECHILD) -- the process is
  alive and reaping works. The loop prints `LOOP i=0 t=0.1` and then nothing for 112s. The only 1s
  `WaitForSingleObject` in the log is the TICK thread's own `time.sleep(1)`
  (`[diag-wait-dur] ... requested=Some(999.998ms) result=Ok(TimedOut)`, exactly one per tick), so the
  loop's `epoll_wait(1000ms)` never re-arms. `[diag-lockstall] raw mutex wait still blocked ... val=1
  now=1 holder_pid=0 holder_tid=0 waited_s=30/60/90/120` on two locks: `WaitState` parks with NO holder --
  a LOST WAKE, not a deadlock. And the freeze starts at t=0.1s, BEFORE the child exits (t~1.0s).
- **sub5** (no Xvfb; arms: bare sleep loop, `sh -c 'sleep 2'`, `dd bs=1024 count=200`, `xset` without
  DISPLAY) isolates what about the child triggers it. Result: see `AGENTS.md`.

Conclusion shape: spawning a subprocess makes the parent's event loop park in a `epoll_wait` timeout wait
that is never woken (`holder_pid=0`), while every other thread in the same host process stays healthy.

## Chromium, long form

- **rc=133 (SIGTRAP) is a `HOME` problem.** Chromium runs as uid 911 (`setpriv --reuid 911 --regid 911
  --init-groups`) and derives its crashpad database from `HOME`; with `HOME=/config` (root-owned 0755)
  `chrome_crashpad_handler` is exec'd with EIGHT arguments and no `--database` among them
  (`--monitor-self`, 4x `--annotation=`, `--initial-client-fd=5`, `--shared-client-connection`), answers
  `--database is required`, the client's handshake read fails (`incorrect payload size 0`) and the browser
  dies on a CHECK. The identical command line with a writable `HOME=/tmp/cuhome` exits 0 and dumps the DOM
  -- reproduced headlessly (`.wfgy/cpad1.sh`, which also prints the real argv by putting a wrapper script
  in front of the handler). `--crash-dumps-dir` is NOT the lever: `grep -a -o crash-dumps-dir
  /usr/lib/chromium/chromium` finds nothing in Debian's build.
- `--user-data-dir` must be `chmod 777` when a root shell created it, else `SingletonLock: Permission
  denied` = exit 21.
- `SHARED_UNIX_CONN_CAPACITY` 1024 -> 4096 (`7d2a6a7`) is what unblocked rendering: chr20's census read
  `occupied=1024 held_live=1024 one_host=1009 multi_host=15` and 303 refusals ALL `reason=shared unix
  connection table full`. The slot shrank ~14.8KB -> ~7KB to fit one power-of-two arena region
  (`SHARED_UNIX_CONN_BUF` 4096->2048, `RING_FD_MAIL_ENTRIES` 4->2); `SharedByteRing` is const-generic over
  `RING_BYTES` and the pty keeps 4096 via `PTY_RING_BYTES` + the `PtyRing<Platform>` alias (a const generic
  argument must be a type or a BRACED const -- unbraced path is E0573). Arena 64 -> 128MiB is free: the
  reservation is `SEC_RESERVE` and `shared_kernel_arena_alloc` commits on demand.
- Still open: the launcher thread SERIALISES cross-process spawns (children can hit the 15s "no connection"
  self-termination); `--single-process` dies on a Chromium CHECK in
  `RenderProcessHostImpl::GetProcessHostForSiteInstance`; ~1 per run `rebuilding a carried SCM_RIGHTS fd
  failed errno=ENOENT spec=F|.../Local Storage/leveldb/LOG`; `Vmem::duplicate` skips `VM_OWN_FORK_PADDING`
  (the SAME-process fork); GWP-ASan `MapRegion` EEXIST occasionally.

## Selkies, long form

- **selk6**: from inside the guest, `curl 127.0.0.1:8081` returned `code=000 size=0` and then `Could not
  connect to server` for the rest of the run, while a plain `python3 -m http.server 8082` answered on every
  probe. (Explained by the 8080-vs-8081 port mismatch below, not by a stall.)
- **selk8**: 8081 with defaults and 8083 with `--printing-enabled=false` both logged `Selkies server running
  on http://0.0.0.0:808X` and then answered nothing, so `start_server()`'s printing block (the inotify
  SpoolWatcher + the cupsd subprocess) is innocent. `/usr/sbin/cupsd` does exist in this image.
- **selk9**: watchdog's inotify Observer runs fine; `asyncio.create_subprocess_exec` works; the same spawn
  WITH `preexec_fn` fails (`SubprocessError: Exception occurred in preexec_fn`).
- **selk10/selk11/selk12**: selkies needs `DISPLAY` exported (it exits silently without one); a log file it
  writes under `/tmp/` is INVISIBLE to the shell that launched it (`wc -l` over a running server's log reads
  0) -- pipe the child's output instead: `( selkies --debug 2>&1 | sed 's/^/[selk] /' ) &`. `kill -9` on a
  wedged cross-process child does not reap it, so A/B on one port is useless (run B reports `Address already
  in use`).
- **aio9 (a corrected measurement error)**: four server arms on one guest (uvloop +
  `asyncio.start_server(sock=)`, uvloop + `start_server(host,port)`, uvloop + aiohttp `SockSite` on a
  pre-bound fd, plain `SelectorEventLoop`), each probed twice: 8/8 `code=200`, 57-244ms. The runs that
  "proved" otherwise (aio5-aio7) used `loop.create_server(h, ...)` with an `(reader, writer)` callback --
  `create_server` takes a PROTOCOL FACTORY called with no arguments, so every accept raised
  `TypeError: h() missing 2 required positional arguments` and uvloop closed the connection. That is also
  why no `epoll_ctl(ADD)` appeared for the accepted fd. "Accept lands seconds late" was likewise wrong: the
  litebox log clock starts at runner start and the guest script's `t0` is ~8s later.
- Guest source: `.litebox-cache/sha256_669f2c..._v2.tar` under
  `lsiopy/lib/python3.13/site-packages/selkies/` (an older 1.x tree sits in `sha256_3e6fd1..._v1.tar`).
  Extract with python `tarfile` into `.wfgy/selksrc/` (140 `.py` files).

## The external fault watchdog (audited 2026-10-03)

`run_external_fault_watchdog_child` (`process_fork.rs:5185`, external process) and
`fault_terminate_watchdog_thread_body` (`lib.rs:5234`, in-process thread) are enrolled for EVERY host
process that reaches `main()` (`main.rs:28` runs before the `is_diagnostic_resume_child` check at
`main.rs:60`), so the root runner and every cross-process fork child are watched.

Criterion (`process_fork.rs:5236-5336`): poll 500ms; `stalled_ticks++` whenever `GetProcessTimes` CPU delta
since stall start is <= `MEANINGFUL_CPU_DELTA_100NS` = 100_000 (10ms); at `grace_ticks` = 30 (**15s**) ->
`TerminateProcess(handle, 1)` (`:5334`). Gated on the sticky manual-reset event
`Local\litebox-fault-armed-<pid>` (`:5041`), set only by `mark_fault_terminate_armed()` (`:5047`) from the
two VEH self-terminate paths (`lib.rs:2319` 64 repeated unrecovered AVs; `lib.rs:2623` unrecovered AV ->
`RaiseFailFastException`). A NULL `OpenEventW` handle means ALWAYS ARMED (`:5243`, `:5255`). Exit code 1 has
no `0xC0DE` marker, so `decode_cross_process_exit_status` returns `Signal(SIGKILL)`
(`litebox_shim_linux/src/syscalls/process.rs:390-394`) -> bash "Killed", rc=137.

A healthy `Xvfb` blocked in `select()` meets the CPU criterion, so this IS a latent false positive -- but it
is NOT what killed the sub1/sub2/sub3 Xvfbs: neither log contains a single `unrecov-av`/watchdog line
(nothing was ever armed) and the deaths coincide with the harness cap (`guest2.ps1:26-28`,
`taskkill /F /T` + `Stop-Process -Force` on all `litebox_runner*`, fork children included).

Env: `LITEBOX_DIAG_NO_EXTERNAL_FAULT_WATCHDOG` (`process_fork.rs:5078`), `LITEBOX_DIAG_NO_FAULT_WATCHDOG`
(`lib.rs:3407`) -- both `var_os`, read once per process at startup (inherited by fork children, so a
harness-level export covers all); `LITEBOX_DIAG_WATCHDOG=1` enables tick prints.

# Overflow from the 2026-10-03 `AGENTS.md` compaction (32,407 -> <28,500 bytes)

Long form that was standing in `AGENTS.md` immediately before this compaction and is not already
reproduced above. `AGENTS.md` is the index; this is the trail.

## Fixed-list, with mechanism (AGENTS.md keeps these as one-liners)

- `5b7ff62` `ShimTransport::connect` dropped `net_lock` before its `spin_loop` (see the freeze section).
- `16f3e76` the whole-guest freeze. Also clamps the platform wait chunk to >=1ms: a sub-ms chunk truncated
  to 0 and `WaitForSingleObject(h, 0)` returns `WAIT_TIMEOUT` WITHOUT blocking, so the loop busy-spun.
- `42c6099` net: hand queued bytes to smoltcp before `close(2)` decides to defer.
- `a1423ee` a carried "child writes" pipe is a local pipe + a pump, so `write(2)` returns before the bytes
  reach the parent -- a bulk producer that exits at once lost its tail (`dd bs=1024 count=200 | wc -c` =
  151552 of 204800). The child's exit now waits for its write pumps to drain (bounded 5s).
- `30d8f43` a `Source` pipe bridge waited for `owners()==1`, but HOLDING an end is not READING it: a wrapper
  (`timeout`/`env`/`nohup`/`setpriv`) keeps stdin open for the child it forks -- deadlock.
  `ForkPipeBridge::pending_bytes` (FIONREAD) breaks the wait when a non-zero count sits unchanged 500ms.
- `7d2a6a7` `SHARED_UNIX_CONN_CAPACITY` 1024->4096 and every unadopted-carry discard path releases the
  sender's slot hold: chromium renders a page with its OWN sandbox active. `ad2659f` `SECCOMP_RET_TRAP`
  returns the syscall number; a connected endpoint's `Conn` hold ships with the carry spec.
- `067367b` fork children keep committed fork padding (`VM_OWN_FORK_PADDING`); cross-process `/proc/<pid>`;
  no-restorer frames refused; AV heals bounded. `19eab93` PID namespaces; crashpad (`yama/ptrace_scope`,
  `PR_SET_PTRACER`, SCM_CREDENTIALS); `CLONE_FS`/`CLONE_FILES` clones stay same-process. `3bfe283`
  `CLONE_NEWUSER` credentials cross fork. `883cab4` user namespaces (id maps, `setgroups` first). `e608959`
  real `chroot(2)`. `2bb71cb` real `seccomp(2)` (classic-BPF on the raw nr at the top of `Task::do_syscall`).
- `668d084` `idle_trim.rs` (`LITEBOX_IDLE_TRIM=0` off). `f271ed2` flat rootfs index. `d428acd`
  `with_root_identity` takes the per-thread root guard too. `a2ebff6` CLOEXEC survives a pipe-end fork
  carry. `748c4e5` registry + record locks out of the shared arena. `f36621a` one `VirtualQuery` per
  region: fork 3-5s -> ~0.5s. `582cc4b` chromium profile dirs use the shared spill.
- `9014df7` unix `EPOLLOUT` only when the blocked message fits -- **reports "chromium headless prints
  example.com"**, not reproduced since.
- Older: `a42d9d0` pipeline EOF; `d944c66` net_lock not orphaned on exit; `f594693` mutable `Credentials`;
  `2b7d7df` TCP `mark_peer_closed()`; `290d4d4` `mprotect` rounding; `3ee1ce7`/`bff1d0b`/`a61ed74` lock
  dead-holder recovery; `0cda0ec` lazy range + `epoll` re-poll; `d383f90`/`d1dae93` pty; `820c2d6` watchdog
  arming; `bb518ca` per-fork env vars consumed by `take_fork_env` -- **any new per-child env var needs a
  remove after adoption**. Memory (`b11f674`, `6fd27b7`, `668d084`): fork child 41MB->8MB; stack private
  resident 2246MB->1222MB; idle trim 2.8GB->0.4GB. Beware root-vs-child heap layout exposing shared-struct
  host pointers (`bootstrap_process`).

## Freeze: audited non-problems, tooling, ruled-out hypotheses

Audited and NOT a problem: the reverse ordering (descriptor table/entry -> `net_lock`) exists nowhere --
fork's fd export runs before `net_lock` is taken (`lib.rs:4173`, `process.rs:4614`);
`resolve_scm_rights_fds` drops its table guard before `scm_carry_spec` (`net.rs:2094` vs `:2171`);
execve's `close_on_exec` drops `rds` first (`file.rs:3565`); exit teardown too (`lib.rs:1946`).

`chrD*.err.log` is BINARY to gm codesearch (NULs) -- use `.wfgy/logscan.py <file> <term...>` or inline
python; `.wfgy/scan_waiters.py` parses an arena `dd` dump. Ruled out: the `close(2)` HUP/linger wait
(`wait_on_events` runs `try_op` FIRST, `polling.rs:59`; only `GracefulIfNoPendingData` parks,
`net/mod.rs:1471`); the TUN thread; panics; waiter-queue overflow.

## Linux runner (`litebox_runner_linux_userland`), long form

Cloud sessions are Linux; there `webtop:debian-xfce` (`--initial-files rootfs.tar --rewrite-syscalls --uid
0 --gid 0 --pid1 --tun-device-name tun0 ... /bin/sh /init`) boots s6, Xvfb, XFCE, nginx, pulseaudio, dbus,
Selkies; a host browser at `http://10.0.0.2:3000` (TUN, host 10.0.0.1) drives it. Harness `tools/webtop/`.
Merged from `claude/modest-feynman-3zpzop` (`764fb30`); not re-verified after the merge. Native fork
(`has_native_fork()==true`): hooks `waitpid`/`waitid(WNOWAIT)`; `SYS_wait4`/`SYS_waitid` seccomp-allowed (a
blocked host syscall answers EINVAL and looks like a hang); `exit_native_fork_child` ends the child with the
guest status; a child COW-copies every private kernel structure, so shared state must live in the arena; the
thread-based fork corrupts guest memory on Linux. Per-process `/proc/self`, real pty line discipline,
SCM_CREDENTIALS, shared-mapping coherence, per-thread root guard, `iter_nowait`, panic = `_exit(134)`,
free >=1MiB returns pages; plateau ~9GB. Added: inotify, `MSG_PEEK` (broke TLS), xattr stubs, tar mtimes,
`mincore`, `copy_file_range`, `splice`, `rt_sigtimedwait`, `O_TMPFILE`, NETLINK_ROUTE dumps,
`pidfd_open`/`waitid(P_PIDFD)`, `getrusage`/`mlock`, ptrace = EPERM. Dead-holder recovery: a waiter blocked
2s checks `tkill(tid,0)`; up to 4 readers tracked. Debug: `LITEBOX_DIAG_FAULT=1 LITEBOX_PRINT_EXE_BASE=1`,
`LITEBOX_DIAG_BIGALLOC=1`, `gdb -p <root> -batch -ex "thread apply all bt"`. Shim tests:
`RUST_MIN_STACK=64M --skip tun`. **Bare `cargo check -p litebox_shim_linux` fails 21x E0433
(`cannot find tracing in __private`) -- pre-existing; use `--features litebox_util_log/backend_tracing`.
`cargo test -p litebox_shim_linux` fails 162/193 with `shared_kernel_arena_alloc_bytes failed` -- also
pre-existing, a Windows test-harness limit.** Gaps: IPv6 rides IPv4; `/proc/<pid>/fd` shows non-path fds as
`anon_inode:[N]`.

## Cross-process shared memory and locks, long form

`RawMutex` = a 32-slot pointer-free wait queue + cross-process `Event`s with `holder_pid` dead-holder
recovery (`bff1d0b`); waits chunk to `LIVENESS_CHECK_INTERVAL` (2s) so recovery can run. A 128MiB
`shared_kernel_arena_alloc` backs `SharedArc<T>` (`GlobalState`, `Network`, pty/unix tables); `SLAB_ALLOC`
stays per-process, and `SharedArc::new` shares only `T`'s inline bytes (a `BTreeMap` has private-heap
nodes), so shared registries are fixed-slot atomic tables. Per-process still: `flock_registry`,
`SafeZoneAllocator`. On Windows every address/TID-based wait is process-local; only a shared kernel object
crosses. `litebox::sync::RwLock` is writer-preferring (`rwlock.rs:74-82`), so a QUEUED writer blocks readers.

## Cross-process file visibility, identity, apt -- long form

A file written by one host process is invisible to siblings until it exits (per-process writable layer).
`syscalls/file_spill.rs` write-through-spills `SPILLED_PREFIXES` to
`%TEMP%\litebox-spill-<rootpid>\<slot>.bin` (1024 slots); open/stat/access/getdents/unlink/rename refresh
the local copy. Prefixes (`file_spill.rs:10-18`): apt's two dirs + five chromium paths. Fixes `apt-get
update`. Extend the prefix list, not the mechanism; `%TEMP%\litebox-spill-*` is never cleaned up.
Direction matters: a cross-process fork child receives the parent's exported writable layer AT SPAWN, while
a child's own writes reach the parent only when it exits and `wait4` imports them
(`litebox-forkwrite-*.tar`). So a fork carry of a fresh `/tmp` fd works and the same fd handed back over
SCM_RIGHTS does not; `only_in_own_writable_layer` is how the carry path tells those apart. The fs layers
check permissions against the calling task's fsuid/fsgid (`litebox::fs::set_effective_identity`); root
bypasses rwx bits. `chown` is real, export/import carry owner and full mode. Anything walking the fs outside
a syscall (export/import) must run inside `litebox::fs::with_root_identity`. `NETLINK_AUDIT` acks every
`NLM_F_ACK` message; `getpriority`/`setpriority` exist (pam_limits aborts on ENOSYS); AF_INET6 answers
`getsockname` etc. as v4-mapped `sockaddr_in6`. Fork children inherit cwd and full credentials
(`task-state:` shim spec, fd `i32::MAX`); a child's deletions reach the parent as `.wh.` tar entries; only
the newest Source pipe bridge of a read end drains.

## Chromium, overflow

Desktop (windowed) variant: `.wfgy/chrdesk.ps1 -Run <n> -MaxSeconds N`. `--publish` works with an ordinary
fork-child server (200/61 bytes seven times, pub2), so a hung published port is NOT automatically a gateway
bug -- ask the guest to curl its own `127.0.0.1:<port>` first. (After an `exec` of the ROOT process the host
listener stops accepting -- published ports are root-process state.) The carry path (SCM_RIGHTS and fork
share ONE, `6beb669`): `scm_carry_spec` returns `Result<Option<String>, &'static str>` and `net.rs` logs
`reason=` with every refusal -- READ THAT FIELD. A connected endpoint, an unbound socket and a listener
cross; a bound-but-unconnected socket, a connect in progress and a bound/connected DATAGRAM socket are
refused EOPNOTSUPP on purpose. Only a `Presence` hold is abandoned; a `Conn` hold ships with the spec. A
file that exists only in the SENDER's writable layer goes out as `T|` (content snapshot), everything else as
`F|` (same inode both sides). Do NOT fix this class by adding `/tmp/` to `SPILLED_PREFIXES`. Repro
`.wfgy/scmring.sh`; `.wfgy/scmlisten.sh` (not run). An SCM_RIGHTS carry the receiver never adopts leaks the
sender's slot hold forever: `UnixSocket::release_unadopted_carry` now runs on every discard path (`net.rs`
EMFILE/rebuild error/MSG_CTRUNC, `file.rs::rebuild_carried_unix`, `UnixSocket::recvfrom`).

## Selkies, overflow

Host BROWSER loads the app: CDP navigation to `http://localhost:8081/` succeeds (120s), 15/15 200, zero
console errors, selkies logs `Client ('10.0.0.1', 49162) connected`, then sits on "Waiting for stream..."
with a black canvas. selk6: from inside the guest, `curl 127.0.0.1:8081` returned `code=000 size=0` and then
`Could not connect to server` for the rest of the run, while a plain `python3 -m http.server 8082` answered
on every probe (explained by the 8080-vs-8081 port mismatch, not by a stall). Guest-side `ps` is useless
here (each cross-process child's `/proc` lists only itself); a host-side process is identified by its THREAD
NAMES: `litebox-guest-pid<N>`. Cheapest full repro: `.wfgy/chrdesk_lock.ps1 -Run <name> -MaxSeconds 660
-Script .wfgy/chrdesk3.sh` (Xvfb -> dbus -> selkies -> xfce4-session -> sandboxed chromium).
`.wfgy/selk6.sh` is the no-DE variant.

## Small facts trimmed from `AGENTS.md` that are not reproduced above

- Cross-process fork child identity: a child's guest pid is the parent's `child_tid`; its Windows pid is
  logged as `winpid=`. `CLONE_FS`/`CLONE_FILES` clones stay same-process (`067367b`).
- `.wfgy/scan_waiters.py` parses an arena `dd` dump. `chrD*.err.log` is BINARY to gm codesearch (NULs).
- `.wfgy/scmring.sh` (run) / `.wfgy/scmlisten.sh` (not run) are the SCM_RIGHTS carry repros; selkies'
  `_run_detached_command` (`websockets_mode.py:4485`) is DEVNULL/DEVNULL + `start_new_session=True`.
- Ruled out for the freeze: dropped cross-process wakes, panics, TUN thread, waiter-queue overflow.
- Shim build: bare `cargo check -p litebox_shim_linux` fails 21x E0433 `cannot find tracing in __private`.

## sub6 / sub7 -- the subprocess wedge, latest probes (2026-10-03)

- **sub6 FALSIFIED "spawning `xset` under uvloop wedges the loop"** (`.wfgy/sub6.sh`): a single
  `create_subprocess_exec("/usr/bin/xset","q")` with NO DISPLAY, followed by 600 `asyncio.sleep(1)` ticks,
  ran clean to the harness cap -- the child exited rc=1 at t~4.2s and the loop printed
  `LOOP post i=412 t=419.6 rc=1` (**419 ticks**). ZERO `[diag-lockstall]` lines in that whole run. The two
  `cdb -pv` snapshots taken mid-run (`.wfgy/sub6.cdb.82144.txt`, `.wfgy/sub6.cdb.121776.txt`) are therefore
  a HEALTHY baseline: python is the 6-thread process (main, epoll_pwait guest thread, idle-trim,
  signal-wake, task-resume-probe, `litebox-guest-pid5`) and the 9-thread one is the root bash runner, whose
  guest thread legitimately sits in `sys_wait4` for the whole run.
- sub5 DID wedge, and the only difference is what came BEFORE the xset spawn: two children
  (`sh -c 'sleep 2; echo done'` and `dd bs=1024 count=200`) spawned, left unread and unreaped. sub5's own
  `[diag-lockstall]` lines name the parked threads: `val=1 now=1 holder_pid=0 holder_tid=0 waited_s=30/60/120
  chunks=15 chunk_ms=2000 rc=258 queued=true occupied=1 pending_wake=false` on
  `tid=ThreadId(7) thread=Some("litebox-guest-pid6")` (python's own guest thread),
  `tid=ThreadId(10) thread=Some("litebox-fork-pipe-pump-parent")`, `tid=ThreadId(2) thread=None`.
- **sub7** (`.wfgy/sub7.sh`) re-runs sub5's shape as four cumulative arms in ONE process, each ending with an
  xset spawn + six ticks: A bare xset; B `sleep2` then xset; C `dd200k` then xset; D both then xset.
  Cumulative by design: arm B already carries arm A's unreaped child, so if the trigger is "an unreaped
  child exists" rather than "a child with bulk unread output exists", B is where it shows. State at the time
  of this compaction (run still in flight): arm A completed clean -- `SPAWNED A-xset pid=8 t=0.2`, 6/6
  `LOOP A-after-xset i=0..5`, `ARM A DONE t=6.3 rc=1`; then `SPAWNED B-sleep2 pid=9 t=6.5` and
  `SPAWNED B-xset pid=11 t=12.8` and the loop stops, with `[diag-lockstall] val=1 holder_pid=0 chunk_ms=2000`
  at t=37/43/67/73s on three threads (`ThreadId(21)`, `ThreadId(7)`, `ThreadId(2)`). No `LOOP_EXC`.
