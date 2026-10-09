# litebox - current state (2026-10-08f; recompact of `-08e`, verbatim in `docs/AGENTS_ARCHIVE_2026-10-08e.md`)

`process.rs`/`file.rs`/`unix.rs`/`epoll.rs`/`mm.rs` = `litebox_shim_linux/src/syscalls/<x>`; `platform/lib.rs` = `litebox_platform_windows_userland/src/lib.rs`; `fork.rs` = `.../process_fork.rs`; **`net.rs` = `litebox/src/net/mod.rs` (NOT `syscalls/net.rs`); `platform/net.rs` = `litebox_platform_windows_userland/src/net.rs`**. This file wins; mechanism prose and verbatim pre-edit text live in `docs/AGENTS_ARCHIVE_*` and `docs/HARNESS-LESSONS.md`.

## Where things stand

- **LINUX-HOST CHROMIUM: THE FORK FAILURE AND THE CARRIED-FD DEATHS ARE FIXED (`7d5c9be7`, `dd94fef9`); 3 RENDERERS NOW START AND STAY ALIVE, BUT NO PAGE EVER LOADS (`dump_dom_lines=0`).** The `Zygote could not fork` was **EAGAIN** (namespace slots held by zombie inits), the 686-deaths/120 s child loop was the carried-fd rebuilds - both fixed BOTH WAYS on one binary. Now: `carry_fail=6` (all `/proc/<already-exited pid>/{statm,status}`, a CORRECT ENOENT), `init_sandbox=1`, `zygote_fail=0`, 3x `Activated seccomp-bpf sandbox for process type: renderer`, NO renderer death. **What still blocks it: after t≈15 s the browser goes SILENT (300 s run: same 20 KB, `rc=124`) while its MAIN thread burns a full CPU at ~16.5k syscalls/s (85% `clock_gettime`, ONE fixed `rip`) interleaved with `ppoll(timeout=0ns)`.** The guest clock is NOT the cause (refuted below). `cargo test --release -p litebox_shim_linux --lib` = **221 passed; 0 failed; 1 ignored**.
  **`/proc/<pid>` ENOENT IS FIXED AND CHANGES NOTHING** (Fixed, newest first): `register_new=16`,
  `proc_task_miss=0`/`proc_stat_miss=0`, yet `Navigate=0`, `WebContents=0`, `dump_dom_lines=0` and
  `gpu_sandbox_warn=1` on BOTH arms. **THE BROWSER MAIN THREAD BURNS ~16.5k syscalls/s (85%
  `clock_gettime` at one fixed `rip` + `ppoll(timeout=0ns)`) AND NEVER ISSUES A NAVIGATION** - that
  loop is what to map next (`file_off = vaddr - base`, recompute the exec `PT_LOAD` base per run).
  **THE MAIN THREAD (tid == pid) MAKES NO `ppoll` AFTER t~16 s** (`sys_ppoll` throttled log,
  `file.rs`): its last ppolls are 0ns/16ms/868ms on fd 11 (eventfd) + fd 13 (pipe); from ~17 s
  on the only ppolls are tid 17/25/30 on sockets with 5 s timeouts and `ready_count=0` (idle).
  It is never parked >=2 s in any syscall, so whatever it does after 16 s never blocks.
  **`LITEBOX_DIAG_MAINTHREAD=1` (throttled, main thread = tid==pid) shows the spin is `clock_gettime`
  (228) and it is on MANY processes' main threads, not the browser's alone**: pids 12, 33, 44, 53, 72
  all sit in `clock_gettime` (the browser's own main thread only shows brk/mmap/mprotect/access).
  NO native control is obtainable here: only the bare binary was extracted (no `icudtl.dat`/`.pak`),
  and a native run dies `rc=133` at `Invalid file descriptor to ICU data received`.
- **GOAL (sandboxed chromium - its OWN sandbox, no `--no-sandbox` - visible in a HOST BROWSER):
  MET `4281283`; RE-PROVEN by chrF35 (`9cd6327`+): DevTools 200 at t=15 s, CDP `vis:"visible"`/
  `rs:"complete"`, `Page.captureScreenshot` 800x600 blue=99.78%, NO `No usable sandbox` in stderr.**
  Decode such a PNG with PowerShell `System.Drawing`+`GetPixel` (`Read` returns NOTHING).
- **ONE SHARED ACCEPT QUEUE PER PORT (`57b87eb`, PROVEN by chrF34): a listening port's backlog is
  `Network::listen_queues` - ONE row per port in the arena, maintained by ANY process's tick - not
  `TcpServerSpecific::socket_set_handles` inside ONE process's descriptor entry** (a parked
  `descriptor_table_mut()`, an `iter_mut_nowait` skip, or that process exiting left the port deaf).
  chrF34: `maintained=[8081, 8082, 9222]` x23,870 from EVERY process; refusals 2 vs chrF32's 22.
- **THE PUBLISHED-PORT FAILURE IS FIXED (`7efefc5`)**: `pump_tcp_flows` gated its graceful FIN on
  `!socket.is_open()`, and smoltcp's `is_open()` is TRUE in CLOSE-WAIT, so the guest parked in
  FIN-WAIT-2 with the reply queued; pub8 0/63 -> pub9 116 OK, pub10 152 OK/1 fail. **RULE:
  `state=CloseWait` + `recv_queue=N` = the reply is here and the host has NOT been told it ended.**
- **APP CENSUS: 21 of 22 ARMS PAINT ON A SETTLED DESKTOP; `xvidtune` IS THE X SERVER, NOT
  LITEBOX** (missing `XFree86-VidModeExtension` on ":1"). **RULE: settle ~120 s after `xfdesktop`
  (90 s is NOT enough), warm-up arm first, 45 s per arm, and SLICE IT (~5 arms/run).** Only catch: `xman` needs `/tmp/man/man1/hello.1`.
- **THE IMAGE TAG IS A MOVING TARGET** (webtop re-pushed 2026-10-05): compare a run's layer
  digests before calling a chromium change a regression; a restore named `.layers.OLDGOOD.json`
  FAILS SILENTLY - the pin is the copy over `ref_docker.io_linuxserver_webtop_debian-xfce.layers.json`.
- **Branches**: THERE IS ONE BRANCH, `main`. `inetfix` was merged at `efa9a4db`, reappeared on the remote (`e4f749d`) and is merged again - delete it, never work on it. **`drmevdev` DOES NOT EXIST on this remote; `ae6926d` is not in this repo.** Land on `main`. **COMMIT AS `lanmower`** - the tree's default identity is `anentrypoint`; amend if a commit lands under it.
- **Before ANY run**: sweep runners (on Windows by CIM - `Stop-Process -Force`/`taskkill` FAIL on
  orphaned fork children); close other browsers; free RAM at the gate.

## Linux-host test suite (this container) - separate stream from the Windows harness above
`cargo test` HERE (kernel 6.17, **no `/dev/net/tun`**, `CapEff: 0`, ~4.2 GB of 7.8 GB). **A test binary
is a HOST process: a panic, an abort or an OOM ends the whole run, not one guest.** Full notes in
**`docs/LINUX-TEST-SUITE.md`**.

- **NOW: `-p litebox_shim_linux --lib` = `222 passed; 0 failed; 1 ignored`**; `--release` = `221 passed`.
  The gap is `process.rs`'s `#[cfg(all(target_os = "linux", debug_assertions))]`
  `test_sigint_with_custom_handler` - intentional. **`cargo test --no-fail-fast` = 58 targets ok / 0
  failed.** **`dev_tests` is a SOURCE ratchet, not a guest test** (`ratchet.rs`: counts line-initial
  `static`/`transmute`/`MaybeUninit` per crate) - it FAILS on any INCREASE, even in a `cfg`-gated file.
- **GREEN: `cargo build`, `cargo check --all-targets`, `cargo clippy --all-targets --all-features` on the
  DEFAULT MEMBERS (what CI runs), `-p litebox_runner_optee_on_linux_userland --all-targets`.**
  **`cargo check --workspace` (ALL members) STILL FAILS HERE BY DESIGN on `litebox_runner_lvbs` (`E0152
  panic_impl`) and `litebox_runner_snp`** - both need a custom target plus `-Z build-std` and a nightly;
  never "fix" them for the host target. **Build ONLY `-p litebox_runner_linux_userland`; never pipe a build through `tail`; cargo cannot relink while a runner holds the exe (`os error 5`).**
- **A MISSING HOST CAPABILITY IS A SKIP, NEVER A FAILURE OR A HANG** (`syscalls/tests.rs`).
- **`LinuxShimBuilder::build` RUNS ON AN 8 MiB THREAD, NOT ON LIBTEST'S 2 MiB** (`tests.rs`
  `BUILD_STACK_SIZE`): `GlobalState` is 796,024 bytes built BY VALUE on the stack (`tests/loader.rs`
  shares that thread). **RULE: a construction with a giant stack frame goes on its own thread with a
  real stack.**
- **THE LINUX ARENA IS RECLAIMED - `create_shared_kernel_state` NOW RECYCLES ITS BLOCKS**
  (`litebox_platform_linux_userland/src/lib.rs`). One `build` cost ~47.9 MiB: the suite peaked at
  **5107 MiB and was SIGKILLed at 204 of 221**; **after: 232.5 MiB.**
- **A LOCK TABLE WITH NO MEMORY TO HOLD IT DEGRADES, NEVER PANICS** (`file.rs`):
  `SharedFlockTable`/`SharedRecordLockTable` hold `Option<&Region>`; with none every operation takes the
  "cannot key this lock" path `excludes == false` already took. Both `.expect("fallback allocation
  failed")` are gone. **Proved by `degraded_lock_table_tests`: one test per table, each with a CONTROL
  on a live table.**
- **A FAULT PROBE MUST NOT READ ACROSS A PAGE BOUNDARY** (`litebox_platform_linux_userland/src/lib.rs`
  `is_syscall_trap`): `rip & 0xfff != 0` confines the `ICEBP;HLT` `rip-1` read to the page already proven
  mapped; at offset 0 it faults a SECOND time inside the handler with SIGSEGV blocked and the thread is
  lost - which is where the synthesized sigreturn trampoline sits, so EVERY delivery through it hung.
- **GLIBC 2.42+ `tcgetattr` IS `TCGETS2` (`0x802c_542a`), NOT `TCGETS`** - so `isatty()` answered EINVAL
  on a real pty and a guest `python3` never went interactive. `litebox_common_linux` now carries the
  44-byte `Termios2` + `TCGETS2`/`TCSETS2`/`TCSETSW2`/`TCSETSF2`; `stdio_ioctl`/`pty_ioctl` serve it.
  **BOTH WAYS: guest `tcgetattr` errno 22 -> 0.**
- **HOST RAM DECIDES THIS SUITE**: `test_dynamic_lib_with_rewriter`/`test_static_exec_with_rewriter` die
  `signal: 11 (SIGSEGV)` on the RUNNER whenever MemAvailable is ~1.2 GB. At >=3 GB: **58 targets ok, 0
  failed**. **`tests/efault.c` needed `#include <stdlib.h>`**; **the pty helpers' deadline is 60 s, not 10**.
- **THREE EXPECTATIONS WERE WRONG, NOT THE SHIM** (prose `-07q`): `getcwd` with no trailing slash, the pty
  slave's **EIO, not EPIPE**, `test_vmm_mapping`'s growth, `RawMutex`'s missing `owner`, `MockPlatform:
  SystemInfoProvider`, `litebox_packager`'s `TarEntry.symlink_target`, `cargo test --release` not
  compiling. **TWO HOST GAPS HAD TO BE CLOSED WITH SUDO**: `libssl-dev` + `pkg-config` (openssl-sys),
  `libclang-dev` (bindgen); `sudo -n apt-get update` first or a stale index 404s.
- **A BINARY OR EXAMPLE IN `default-members` MUST COMPILE ON EVERY TARGET, EVEN IF IT CAN ONLY RUN ON
  ONE** (`litebox_presenter`, `presenter_{bench,smoke}` - `litebox_platform_windows_userland` is
  `#![cfg(windows)]`). **Fix = a TARGET DISPATCHER.** **NEVER run `cargo fmt --all` here**: `--check`
  is RED on **209 pre-existing hunks in ~30 untouched files**.

## Fixed, newest first (RULE + key `file:line` + the repro that proved it)

- **`dd94fef9` A CARRIED FD MUST BE REBUILDABLE IN THE RECEIVER, NOT MERELY NAMED** (`file.rs` `carriable_file_spec_for_raw_fd` / `rebuild_carried_fd`). **RULE: a spec the receiver cannot open is a dead child - carry the bytes or create the path.** Three shapes answered ENOENT/EACCES and each killed the chromium child handed it (686 in 120 s). (a) An `O_WRONLY` file living only in the sender's writable layer: `snapshot_via_readonly_reopen` now reads it through a temporary `O_RDONLY` handle and restores the SENDER's flags on the spec. (b) EVERY `T|` byte snapshot failed - `rebuild_snapshot_file` created its carrier under `/dev/shm`, which this rootfs lacks; it now delegates to `install_shm_file`. (c) A carried DIRECTORY fd (`F|65536|...`, `O_DIRECTORY`) names a path the receiver cannot see: `mkdir_chain` now runs on ENOENT **and EACCES**, and creates **0777** - under `root_guard` a 0700 dir is root-owned, so an unprivileged receiver was refused the reopen it had JUST created (`chain=dddd created=1 reopened=false`). **BOTH WAYS on ONE BINARY (`LITEBOX_FILE_CARRY_FIX_OFF=1`), 120 s: OFF `carry_fail=1042`, 686 deaths, `init_sandbox=163`, 2.5 MB log; ON `carry_fail=6` (all `/proc/<dead pid>/{statm,status}` = a CORRECT ENOENT), 3 deaths, `init_sandbox=1`, 50 KB log.** **`probe28`: in one process an existing dir opens with `O_DIRECTORY`, anything missing is ENOENT(2), never EACCES.**
- **`7d5c9be7` LINUX DESTROYS A PID NAMESPACE WHEN ITS INIT EXITS, WHATEVER IS STILL UNREAPED IN IT** (`pidns.rs` `PidNamespaceTable::{create,init_exited}`, `process.rs` `do_clone`/`prepare_for_exit`). The zygote forks every renderer/utility into its own `CLONE_NEWPID` namespace and reaps a child only when the browser asks, so each init stayed a zombie holding its slot: `in_use=256 sealed=256` after ~62 s, then `create` returned `None` -> **EAGAIN**. `create(parent, reclaim_dead)` reclaims dead slots and retries once; `init_exited(ns)` runs when `ns != INITIAL_NS && ns_pid == 1`. **BOTH WAYS (`LITEBOX_PIDNS_RECLAIM_OFF=1`): OFF `zygote_fail=1`, `init_sandbox=80`, 1 `Exception(3)`; ON `zygote_fail=0`, `init_sandbox=196`, 0 exceptions, 640 namespaces destroyed with their init.** **`2f6ff32240` fixed a DIFFERENT `Zygote could not fork` (EPERM) - get the errno before re-deriving.**
- **`6985804e` A CARRIED FD MUST PRESENT THE ACCESS MODE `fcntl(F_GETFL)` REPORTS** (`file.rs`: `carriable_shm_for_raw_fd` uses `regular_file_getfl`). A `/proc/self/fd/N` reopen of an unnamed file is a `dup`, so the descriptor KEEPS `O_RDWR` in its open flags and its read-only-ness lives ONLY in `ReopenedAccess`; the `S|` carry spec read the RAW flags, so every Mojo `ScopedFDPair` arrived with BOTH halves `O_RDWR` and `PlatformSharedMemoryRegion::Take()` failed -> `IMMEDIATE_CRASH` -> GPU respawn loop. **BOTH WAYS (`LITEBOX_SHM_CARRY_GETFL_OFF=1`): OFF 20/20 `accmode=2`, `init_sandbox=1`, 30 KB; ON 5670 `accmode=0` of 6409, that rip NEVER appears, `init_sandbox=82`.** **`diag-shmcarry`'s `accmode` histogram IS the discriminator.**
- **EVERY GUEST PROCESS MUST BE IN THE FORK-FAMILY REGISTRY OR `/proc/<pid>` IS ENOENT** (`process.rs` `reinit_as_native_fork_child`; `signal/xproc.rs` `xproc_preregister_child`, `xproc_register_local`, `reclaim_dead_hosts`): a platform with no cross-process signal delivery reports host pid `0`, and BOTH registration sites were gated on a NONZERO host, so `SharedProcessTable` was EMPTY on the Linux host and `pid_is_known()` was false for everything -- `/proc/<pid>/{stat,task,status}` answered ENOENT in the browser for renderers that were plainly alive (pid 82 logs `Created context:` in the same second as `/proc/82/stat errno=2`). Register under host `0` = "host unknown", which `reclaim_dead_hosts` now leaves alone; `xproc_slot` is still published only for a REAL host pid, so signal delivery is untouched. **BOTH WAYS by `LITEBOX_PROC_PID_TABLE_LEGACY=1` on ONE binary: fix on = `register_new=16`, `proc_task_miss=0`, `proc_stat_miss=0`; legacy = `register_new=0`, `proc_task_miss=10`, `proc_stat_miss=14`.** **NOT THE PAGE-LOAD BLOCKER: `dump_dom_lines=0` and `gpu_sandbox_warn=1` on BOTH arms.**
- **`a3c3bb17` `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, NULL)` ANSWERS EFAULT, NOT EACCES** (`seccomp.rs` `sys_prctl_set_seccomp`): Linux copies `sock_fprog` out of userspace BEFORE it checks `no_new_privs`. **Chromium's `KernelSupportsSeccompBPF()` probe reads exactly that errno as "this kernel has seccomp-bpf"** → the renderer takes its own unsupported-sandbox `IMMEDIATE_CRASH`. **BOTH WAYS by tr6/tr7**: before, renderers 72/73/74 died `Signal(5)` at t=26 s; after, `result=Err(EFAULT)` for every tid and **0 fatal signals**.
- **`2f6ff32240` `clone(CLONE_NEWPID|SIGCHLD)` NEEDS NO `CAP_SYS_ADMIN`** (`process.rs`): the zygote forks with arg0 **`0x20000011`** and NO `CLONE_NEWUSER` from an unprivileged uid, so the clone answered EPERM. **`zygote_fail` 110 -> 1.** **BOTH WAYS by `probe20`**: before, `clone(SIGCHLD|CLONE_NEWPID)` is the ONLY variant answering EPERM; after `pid=4 ... code=7`; NEWNS/NEWNET/NEWUTS/NEWIPC/NEWCGROUP and `unshare(CLONE_NEWPID)` still EPERM.
- **CHROMIUM'S UNREACHABLE PADDING IS `IMMEDIATE_CRASH()` = `int3; ud2`** (`cc 0f 0b`). int3 is TRAP-class so the reported rip is the NEXT byte: **one crash rip yields BOTH `Exception(3)` and `Exception(6)`**. So "the rip is in padding" means a NORETURN call returned OR a CHECK fired. Slice with `dd` + `objdump -D -b binary -m i386:x86-64 --adjust-vma=<vaddr>` (seconds, vs minutes on the 327 MB binary).
- **GUEST FILES ARE NOT HOST-VISIBLE.** `/tmp/shot.png` written by a guest lives only inside litebox. **To prove a render, run `/usr/bin/python3 -u -c <script>` as the program** and let the GUEST decode its own PNG - `--initial-files` puts `usr/bin/python3` in the rootfs even though there is no `/bin/sh`.

Mechanism prose for every sha is in `docs/AGENTS_ARCHIVE_*` (newest `-08b` ... `-09-03`); this file keeps the RULE, the `file:line` and the proof.

- **`e4f749d` A LISTENING PORT BELONGS TO EVERY LIVE REFERENT, NOT TO THE PROCESS THAT ARMED IT** (`net.rs` `ListenQueue::ref_pids` + `release_listen_queue`): retire only when NO referent pid is live AND `refs` is spent. **BOTH WAYS by lqref1: new `CONNECT_OK body=b'CHILD'` x3 / `retire=false`; `LITEBOX_LISTEN_QUEUE_LEGACY_RETIRE=1` `CONNECT_FAIL` x3.** **A row with NO referent pid is NEVER retired (pid 0 = unknown, not dead).**
- **`eee620f` AN UNENUMERATED HOST REFUSAL IS AN ERRNO OR A SIGNAL, NEVER A PANIC**: console stdio failure + a dead console-pump thread = EIO/EOF (`devices.rs`); `exception_handler` maps GUARD_PAGE/STACK_OVERFLOW/IN_PAGE_ERROR -> SIGSEGV, SINGLE_STEP -> SIGTRAP, FLOAT_* -> SIGFPE, any other code = a throttled `error!` + SIGSEGV; `dup`'s unfilled slot = EMFILE. **DEFENSIVE - NOT PROVED BOTH WAYS.**
- **`61fb111` A FILE SIZE THIS STORE CANNOT BACK ANSWERS EFBIG, NEVER A PANIC** (an in-mem file's bytes are ONE contiguous host allocation). **RULE: a guest number that becomes an allocation size must be validated at the SYSCALL boundary - `try_reserve` alone does NOT help.** **BOTH WAYS by panicx1: before, the run dies at `memfd_big` (`WORKER_RC=137`); after `memfd_big`=EFBIG(27) + `pwrite_far`=EFBIG(27), 10/10 OK.**
- **`5dce756` A SECOND WAVE OF SYSARG PANIC SITES ANSWERS AN ERRNO, NEVER A PANIC**: `setsockopt` on the wrong socket type, a short AF_UNIX `addrlen`, a failing `accept`/`socketpair` close, a `ppoll` `nfds` from the guest, `clone(CLONE_SETTLS)` out of range, `execve` TLS clear, an exhausted `SharedUnixConnTable` arena. **panicx1: 10/10 OK (`keepintvl_udp` 92, `ppoll_bignfds` 22, `clone_badtls` 1).**
- **`0095f6f` AN UNSUPPORTED SOCKET TYPE/DOMAIN, `prctl`/`arch_prctl`/`fcntl` COMMAND OR ROBUST-LIST PID ANSWERS AN ERRNO, NEVER A PANIC**: INET `SOCK_SEQPACKET` = EPROTONOSUPPORT, a domain with no socket = EAFNOSUPPORT, `get_robust_list(<other pid>)` = EPERM, an unknown command = EINVAL, a PI futex = ENOSYS. **BOTH WAYS by sockx1: before `panicked at syscalls/net.rs:1345` + `rc=137`; after 10/10 OK.**
- **`8635867` A SYSCALL ON THE WRONG FD KIND ANSWERS AN ERRNO, NEVER A `todo!()` PANIC**: `ftruncate`/`fallocate` on a socket or pipe = EINVAL, `fcntl(F_SETFL)` on an epoll fd = accepted, an unhandled stdio ioctl = ENOTTY. **truncx1: `file rc=0 size=4`; pipe+socket `rc=-1 errno=22`.**
- **`4fbbf9e` A RENAME MOVES THE STORE'S ROW, NOT THIS PROCESS'S COPY OF THE FILE** (`move_slot` `file_spill.rs:173`). **BOTH WAYS by renamecoh1 - the writer is STILL RUNNING when the rename lands: `on` `moved=104 a=104`; `off` `moved=0` BAD.**
- **`8f33179` AN `O_APPEND` WRITE IS PLACED BY THE STORE, NOT BY THE WRITER'S OWN COPY** - `O_APPEND` must be recorded per fd (`fd_append`, set at open). **BOTH WAYS, ONE BINARY: `LITEBOX_SHARED_WRITE_PREFIXES=/tmp/co/` = `concurrent 400 (a=200 b=200)`; without it `concurrent 200 (a=0 b=200)` BAD.**
- **`7028ef5` TWO HOST PROCESSES WRITING ONE FILE CONVERGE THROUGH THE SHARED WRITE STORE.** `LITEBOX_SHARED_WRITE_PREFIXES` shares any host-named path on top of `SPILLED_PREFIXES`; `sync_spilled_fd` pulls the store's bytes in before EVERY read/write; `install_spilled_content` asks for bytes BEFORE truncating. **BOTH WAYS: `lockapp1 contend` `spillx1` `len=2000 A=1000 B=1000` vs `len=1000 B=0`.**
- **`722d8c0` A GUEST STARTS WITH ITS IMAGE'S OWN `Env`, AS `docker run` DOES.** `oci.rs` records `config.Env`; `LITEBOX_IMAGE_ENV_OFF=1` restores the empty env. **BOTH WAYS by exeprobe3: ON `ENV_PATH='/lsiopy/bin:...'`; OFF `ENV_PATH=None`.**
- **`08b1856` A GUEST-REACHABLE LENGTH OVERFLOW ANSWERS AN ERRNO, NEVER A PANIC.** `sys_madvise`/`sys_mprotect` round with `checked_next_multiple_of`. **mmapx1: `madvise(len=2^64-1)` = `rc=-1 errno=22`.**
- **`67b198f` A POSIX RECORD LOCK EXCLUDES ACROSS HOST PROCESSES.** `SharedRecordLockTable` (256 rows, arena, keyed `(dev,path)`, dead holders reclaimed by host-pid liveness); `LITEBOX_RECORD_LOCK_SHARED_OFF=1` restores the old path. **BOTH WAYS: reclock1 `CONTEND_FIRST=errno=11`/`WAIT_ELAPSED_MS=3873`; reclock1off `OK`/`0`.**
- **`7efefc5`** see Where things stand; it instruments the inbound path: **`diag-pump`** (+`-write`/`-read`) with `state`/`recv_queue`/`send_queue`/`to_real`/`to_guest`/`real_eof`/`fin_sent`. **`SocketSet` has NO `len()`** - `.iter().count()`. **`00d0c3c` A CROSS-PROCESS FORK CHILD EXPORTS WHAT IT CHANGED, NOT ITS WHOLE LAYER** - a PATCH: identical entries withheld, growth as a TAIL, else BLOCK RANGES; kind in ustar `gname`. **An unmarked payload still replaces the file.**
- **`340fd98` AND OLDER** (mechanism prose verbatim in `-07q`/`-08a`): `340fd98` a `MADV_DONTFORK` range is
  withheld from a cross-process fork child; `9b0823f` a connect verdict names the port it dialled;
  `3b0fcfe` a verdict carries `state=`/`closed_here=`/`slots=`/`sockets=` - a non-timeout closure is NOT
  proof an RST arrived; `150e6e0` an SCM_RIGHTS carry that posts no fd mail gives its hold back;
  `eb16abf`/`04e9961` SUPERSEDED by `57b87eb` (only ONE process polls the IP interface); `ca78f75` the
  accept queue belongs to the ENDPOINT, not the process; `889351a` an INET socket is nameable for
  SCM_RIGHTS (`N|`), EPOLL STAYS REFUSED ON PURPOSE; `0e45b67`/`884079f`/`daaff53` a cloexec unix socket,
  an epoll set and a bound-but-unconnected AF_UNIX cross a cross-process fork - re-apply cloexec in the
  child (STILL REFUSED: connect-in-progress, bound/connected DATAGRAM); `2b622b4` a shared connection's
  side may never lose a LIVE holder; `f7d7aaa` a host-side refusal is a log line, never an `assert!`;
  `a3aca20` a fork-family socket's rx is pulled by its reader, never pushed by the tick - **the shared
  socket buffer pool is what a desktop runs out of** (`MAX_DATA_SLOTS` 512, `TooManySockets` IS `EMFILE`);
  `39616f4`/`2df5677` accept-queue teardown/re-arm; `37c2374`/`7c1d987`/`7c638ca`/`a2ac697` mapping
  qualifiers travel with the VMA (`VM_PRIVATE_FILE_COW` bit 11 -> `PAGE_*_WRITECOPY`; a `PROT_NONE` file
  mapping is a RESERVATION that still carries the bytes). **Older, binding**: FD_CLOEXEC is about `execve()`,
  not `fork()`; `a629714` a fork child SHARES `MAP_SHARED` - validate every segment BEFORE reserving;
  `f260226` `flock(2)` keyed `(dev,path)`; `16f3e76` no path holding `net_lock` may block; `b012910` never
  refill a chunk no longer `PAGE_NOACCESS`.

## Open, in rough priority order

1. **LINUX CHROMIUM: 3 RENDERERS START AND SURVIVE, BUT NO PAGE EVER LOADS** (fork failure and carried-fd deaths are FIXED). After t≈15 s the browser goes SILENT (300 s run: same 20 KB log, `rc=124`) while its MAIN thread burns a full CPU: **~16.5k syscalls/s, 85% `clock_gettime` at ONE fixed `rip`, interleaved with `ppoll(timeout=0ns)`**; `dump_dom_lines=0`, no crash, no child deaths. **Next: instrument from inside the shim** - `ptrace` is BLOCKED here (gdb: "Inappropriate ioctl for device").
2. **CLOSED - NO LIVE SERVER'S PORT GOES DEAF** (`57b87eb`, chrF34; chrF35's "8081 went deaf" WAS HOST
   MEMORY - chrF36: 27/27 host probes `200 786`, row never retired; xproc41/42: busy 41/41, deaf = live
   owner that never accepts -> refuses at backlog 8 and RECOVERS, vs orphaned -> ~25 ms PERMANENTLY).
   **RULE: `selkpid=none` is a ps ARTIFACT; `slots=none` = no socket on that port at that instant.**
3. **CLOSED - chromium's kills are guest-side**: `Exception(14)`/`0x15` = an instruction fetch of a PRESENT
   non-executable page (`rip` in a `VM_*` range with NO `VM_EXEC`); `Exception(3)`/Signal(5) with
   `cc 0f 0b` at `[rdi]` = chromium's OWN `IMMEDIATE_CRASH`, and it KEPT PAINTING. **A 0-BYTE UNIX READ
   MEANS `peer_gone()`, NOT A CLOSED PEER** (`unix.rs`). `--database=PATH` is REQUIRED. (Prose in `-07q`.)
4. **BYTE STORE: CLOSED FOR BOTH REAL CONSUMERS, FOR RENAME, AND FOR THE NON-APPEND `pwrite` SHAPE**
   (numbers under `7028ef5`/`8f33179`, all BOTH WAYS). **What is left is architectural**:
   `install_spilled_content` is a whole-file `O_TRUNC` replace on first publish; `self.locked(..)` is held
   across `platform.spill_write`; and `FileX { data: Cow<'static,[u8]> }`
   (`litebox/src/fs/in_mem.rs:1530`) is PER-PROCESS - bytes cross only at fork spawn/exit EXCEPT through
   `SharedFileSpill` (`file_spill.rs`). Also open: AF_UNIX exhaustion is silent (256 slots, keys >108
   bytes); `timerfd`/`signalfd` uncarriable; **cross-process-fork state NOT restored**.
5. **THE ARENA: one `create` costs ~48.5 MiB**, so only 2 fit the 128 MiB Windows arena and
   `create_shared_kernel_state` PANICS there; an `attach` costs 0 (1 create / 503 attach). Make `create`
   idempotent by slot, only if creates exceed 2. **The LINUX arena is already RECYCLED.**

## Standing lessons and hard constraints (mechanism: `docs/HARNESS-LESSONS.md`)

- **`litebox::sync::RwLock::try_read`/`try_write` FAIL WHEN A WRITER IS MERELY QUEUED** (`litebox/src/sync/rwlock.rs`): ONE thread parked in a blocking `descriptor_table_mut()` makes every `try_descriptor_table()` fail forever and the per-tick socket sweep a silent no-op in that process. Dead-holder recovery needs the owner thread DEAD; a parked one is never recovered.
- **THE CROSS-PROCESS FORK'S COPY PLAN IS GRANULE-WIDENED AND MERGED** (`process.rs`: `GRANULE = 0x1_0000`; a group base MUST stay granule-aligned or Windows answers 87). **NOTHING IN `litebox/src/mm` ZEROES A FRESH PAGE**. **`vma_layout()` zips `ranges`/`flags`/`executable`/`is_file_backed` POSITIONALLY - filter ALL or NONE.**
- **A GUEST PACKET TO `127.0.0.0/8` OR TO `GUEST_IP_ADDR` IS LOOPED IN-PROCESS** (`phy.rs:166`): a guest's `127.0.0.1:<port>` NEVER reaches the host's published listener. **`data_granted` = CAPACITY granted, `data_used` = slots in use**.
- Guest-reachable code returns an errno, never a panic (the host process IS the whole session). Refusal errno is contract: EPERM degrades, EINVAL/ENOSYS fails hard. `LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check. Never subtract timestamps across a parent and a fork-child log.
- **A `TypedFd` index is valid only against the `Descriptors` that inserted it, and only for its own subsystem.** **A `SharedUnixAddrPresenceTable`-shaped table needs every write path mirrored, and no per-tick sweep may DROP another process's object.** **A backlog slot is reachable by NOTHING but `accept`.** **`(dev,ino)` is cross-process-stable for IMAGE files ONLY** - key shared registries on `(dev,path)`. **`LITEBOX_*` env flags come from the HOST env via `platform.env_flag`.**
- **Host memory: check host RAM FIRST.** gm's browser (2.2 GB PER INSTANCE), the `queue.mjs` chrome storm and `bun` - **none is mine to kill. When free RAM is short there is nothing safe to reclaim - wait.**
- **A process's writes are invisible to everyone until it is REAPED, not when it closes**; a fork child's writes reach the parent only on `wait4`. Cross-process fork carries pipes/regular files/eventfds/pty/unix sockets + INET + `MAP_SHARED`, drops pty fds and non-INET cloexec fds; slots (6) gate the spawn, fail open after 8s.
- Logs: verbosity from `LITEBOX_LOG`, not `RUST_LOG`; **`debug!` fields take `&str`, not an owned `String`**. **Timestamps are PER-PROCESS UPTIME; `.err` lines carry ANSI escapes.** **The `tag=`/`dtag=` fields DO NOT identify a host process.** Socket census is `local_port:state:remote_port`; a listening slot is `0:L:0`, so the census CANNOT prove a port is armed.

## Closed - do not re-attempt without a genuinely new approach

**REFUTED ON THE LINUX HOST (chromium), each by a probe that could have failed - do not re-open without a NEW reproduction:**
(a) **"abort/SIGABRT is broken"** - `probe18`/`19`: `abort`/`raise`/`tgkill`/`tkill`/`kill(self,SIGABRT)` and a real `-fstack-protector-all` smash ALL die on Signal(6).
(b) **"the guest FS base moves, so `%fs:0x28` misreads"** - `probe24`: 200k main + 100k pthread + 200k canary frames: **0 changes**. **`diag-fsbase` (`lib.rs:2419`) is UNRELIABLE**: it reads the HOST `.tbss` slot while the platform SWAPS fs/gs.
(c) **"seccomp traps abort's syscalls"** - all 511 `PR_SET_SECCOMP` calls are `prog=0x0` probes -> EFAULT, **ZERO** `delivering SIGSYS` lines.
(d) **"per-pid `/proc` is missing"** - `probe22`: `/proc/<pid>/{stat,status,comm,task/...}` and `ls /proc` are correct for a live fork child; the ENOENTs are children that ALREADY exited.
  (e) "the frame is corrupt" - `rsp = rbp-0x260` is exactly right; all 5 return addrs are preceded by
  a `call`. (f) "litebox hooks __stack_chk_fail" - codesearch finds no such hook.
Verbatim in `docs/AGENTS_ARCHIVE_2026-10-07k.md`. **REFUTED for 8081: `xproc18`/`19`/`20`; "a busy port goes deaf" (41/41); "an attached host client deafens the port" (37/37).** **CLOSED: `eb16abf`, `150e6e0`, chrF14 window loss, apps10 `BadMatch`, chrF24-27, grey/`NO_PNG`, xproc44, the 23-arm app census.**

- **REFUTED: "the ppoll timeout is not honoured" and "a clock is frozen"** (guest probe30 with a
  NATIVE control, both green): `ppoll` on an empty pipe ret=0 at 1000.2ms/300.1ms (asked 1000/300);
  `epoll_wait` 1000/300 -> 0 at 1000.1/300.2ms; `ppoll(NULL)` blocks 400.1ms until a write; an
  already-ready fd returns 1 at 0.0ms; ALL EIGHT clock ids (0,1,2,3,4,5,6,7) advance over a 300ms
  sleep, REALTIME/MONOTONIC magnitudes 0.300s. litebox == native on every row.
- **`LITEBOX_PIDNS_RECLAIM_OFF` IS NOT THE DISCRIMINATOR for `dump_dom_lines`** (A/B, 80 s/arm, one
  binary): `on` = rc=124, 91785 B, `dump_dom_lines=0`, `seccomp_renderers=4`, `execve_total=5`,
  `exit_group=8`, `zygote_fail=0`; `off` = rc=124, 116016 B, every one of those IDENTICAL. So
  `7d5c9be7` (pid-namespace slot reclaim) is not the cause of "no page ever loads".
## Docs and tooling map
Archives under `docs/` (`-07q`...`-09-03`); `docs/HARNESS-LESSONS.md` = harness prose,
`docs/LINUX-TEST-SUITE.md` = Linux notes. gm `codesearch`
(INVARIANT 4: never grep/find): `literal` is EXHAUSTIVE, `dual` a ranked SAMPLE - never
conclude "absent" from it; scope with `path`/`glob`; **dual SILENTLY IGNORES `path`**.
Spool fallback: fields are FLAT (`query`/`mode`/`path`/`output`/`cwd`/`session_id`), never
nested under `body`; write `in/<verb>/<session>-<N>.txt` atomically, read
`out/<verb>-<session>-<N>.json`.