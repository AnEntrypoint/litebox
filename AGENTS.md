# litebox - current state (2026-10-08b; recompact of `-08a`, verbatim in `docs/AGENTS_ARCHIVE_2026-10-08a.md`)

`process.rs`/`file.rs`/`unix.rs`/`epoll.rs`/`mm.rs` = `litebox_shim_linux/src/syscalls/<x>`; `platform/lib.rs` = `litebox_platform_windows_userland/src/lib.rs`; `fork.rs` = `.../process_fork.rs`; **`net.rs` = `litebox/src/net/mod.rs` (NOT `syscalls/net.rs`); `platform/net.rs` = `litebox_platform_windows_userland/src/net.rs`**. This file wins; mechanism prose and verbatim pre-edit text live in `docs/AGENTS_ARCHIVE_*` and `docs/HARNESS-LESSONS.md`.

## Where things stand

- **LINUX-HOST CHROMIUM: THE GPU-PROCESS RESPAWN LOOP IS FIXED (`6985804e`); `--dump-dom` STILL PRINTS NOTHING.** The loop was `PlatformSharedMemoryRegion::Take()` rejecting EVERY carried Mojo `ScopedFDPair` because the read-only half arrived `O_RDWR` and `Take()` demands `O_RDONLY` on it; chromium answers that with `IMMEDIATE_CRASH` (`rip=0x9133314c2`), so the GPU process died and was respawned ~3x/second and no renderer ever launched. **BOTH WAYS ON ONE BINARY (`LITEBOX_SHM_CARRY_GETFL_OFF=1` restores the raw flags)**, `chrdom.sh`, 150 s each: **OFF** = 20 of 20 `S|` carries `accmode=2`, GPU dies at `rip=0x9133314c2`, `init_sandbox=1`, 30 KB log; **ON** = 5670 `accmode=0` carries (of 6409), that rip NEVER appears, `init_sandbox=82`, 1.8 MB log. **Still open**: no renderer, `dump_dom_lines=0`; the browser runs to t≈71 s and forks 82 sandboxed children, then `Zygote could not fork: process_type utility numfds 6 child_pid -1` -> `NOTREACHED hit. Did not receive ping from zygote child` -> `exit_signal Signal(5)`.
- **GOAL (sandboxed chromium - its OWN sandbox, no `--no-sandbox` - visible): MET `4281283` on the WINDOWS harness, RE-PROVEN on the tip by chrF35 (`9cd6327`+): DevTools 200 at t=15 s, CDP `vis:"visible"`/`rs:"complete"`/`bg:rgb(32,192,240)`, `Page.captureScreenshot` 800x600 blue=99.78%, window `0xa00003 "litebox A-blue" 800x600+40+40`, NO `No usable sandbox` in chromium's stderr. **Decode such a PNG with `System.Drawing`+`GetPixel` (`Read` returns NOTHING).**
- **ONE SHARED ACCEPT QUEUE PER PORT (`57b87eb`, PROVEN by chrF34): a listening port's backlog is
  `Network::listen_queues` - ONE row per port in the arena, maintained by ANY process's tick - not
  `TcpServerSpecific::socket_set_handles`, a `Vec` inside ONE process's descriptor entry.** chrF34:
  `maintained=[8081, 8082, 9222]` x23,870 from EVERY process; refusals 2 vs chrF32's 22.
- **THE PUBLISHED-PORT FAILURE IS FIXED (`7efefc5`); IT IS NOT SELKIES AND NOT A BROWSER.** `pump_tcp_flows` gated its graceful FIN on `!socket.is_open()`, and **smoltcp's `is_open()` is TRUE in CLOSE-WAIT**, so the guest parked in FIN-WAIT-2 with the reply in its `recv_queue` (`diag-pump`: `recv_queue=222`) while the host blocked in `ReadToEnd`. pub8 0 of 63 -> **pub9 116 OK**, **pub10 152 OK / 1 fail**. **RULE: `state=CloseWait` + `recv_queue=N` = the reply is here and the host has NOT been told the exchange ended.**
- **APP CENSUS: 21 of 22 ARMS PAINT ON A SETTLED DESKTOP; `xvidtune` IS THE X SERVER, NOT LITEBOX**
  (`XFree86-VidModeExtension` missing on ":1"); the only app catch is `xman` (`/tmp/man/man1/hello.1`,
  else rc=1). **RULE: settle ~120 s after `xfdesktop` (90 s is NOT enough), warm-up arm first, 45 s per
  arm - 23 arms do NOT fit 1400 s, so SLICE IT (~5 arms/run).**
- **THE IMAGE TAG IS A MOVING TARGET.** `.../webtop:debian-xfce` re-pushed 2026-10-05: every run that
  painted used the OLD layers. **Compare a run's layer digests before calling a chromium change a
  regression.** A restore named `.layers.OLDGOOD.json` FAILS SILENTLY - the pin is the copy over
  `ref_docker.io_linuxserver_webtop_debian-xfce.layers.json`.
- **Branches**: THERE IS ONE BRANCH, `main`. `inetfix` was merged at `efa9a4db`, reappeared on the remote (`e4f749d`) and is merged again here - delete it, never work on it. `origin/main` tracks `main` 1:1. **`drmevdev` DOES NOT EXIST on this remote (no `origin/drmevdev` ref) and `ae6926d` is not in this repo at all** (`666c484`). Keep it branchless: land on `main`, never open another branch. **COMMIT AS `lanmower`** - the working tree's default git identity is `anentrypoint`; amend if a commit lands under it.
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

- **`6985804e` A CARRIED FD MUST PRESENT THE ACCESS MODE `fcntl(F_GETFL)` REPORTS, NOT THE ONE IT WAS OPENED WITH** (`file.rs`: `carriable_shm_for_raw_fd` now uses the new `regular_file_getfl`, which is also what `sys_fcntl`'s `GETFL` arm uses). **A `/proc/self/fd/N` reopen of an unnamed file (a memfd) is a `dup`** (`sys_openat`'s `/proc/self/fd` arm, `file.rs:2633`), so the descriptor KEEPS the original `O_RDWR` in its open flags and its read-only-ness lives ONLY in the `ReopenedAccess` metadata that arm attaches (`file.rs:2644`). `F_GETFL` honoured that metadata; the `S|` shm carry spec read `files.fs.open_flags(fd)` - the RAW flags - instead. **So every Mojo `ScopedFDPair` reached the receiver with BOTH halves `O_RDWR`, and `PlatformSharedMemoryRegion::Take()` (which demands `O_RDWR` on fd1 and `O_RDONLY` on fd2) failed with error 3 -> `IMMEDIATE_CRASH` at `rip=0x9133314c2` -> GPU respawn loop, `renderer=0`.** **BOTH WAYS on ONE BINARY**: see Where things stand. **`diag-shmcarry` is the instrument**: `S-spec from sender descriptor` logs `flags`/`accmode` per carry - that histogram IS the discriminator; `install_shm_file failed`/`ok` name the receiver's step and fd. **Do NOT "fix" the reopen - `probe27` proves it already answers `MODE=0`.**
- **`a3c3bb17` `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, NULL)` ANSWERS EFAULT, NOT EACCES** (`seccomp.rs` `sys_prctl_set_seccomp`): Linux copies `sock_fprog` out of userspace BEFORE it checks `no_new_privs`. **Chromium's `SandboxBPF::KernelSupportsSeccompBPF()` probe reads exactly that errno as "this kernel has seccomp-bpf"** → the renderer takes its own unsupported-sandbox `IMMEDIATE_CRASH`. **BOTH WAYS by tr6/tr7**: before, renderers 72/73/74 died `Signal(5)` at t=26 s; after, `diag-seccomp` shows `result=Err(EFAULT)` for every tid and **0 fatal signals**.
- **`2f6ff32240` `clone(CLONE_NEWPID|SIGCHLD)` NEEDS NO `CAP_SYS_ADMIN`** (`process.rs`): the zygote forks every renderer/utility with arg0 **`0x20000011`** and NO `CLONE_NEWUSER`, from an unprivileged guest uid, so the clone answered EPERM and chromium logged `Zygote could not fork ... child_pid -1` + `NOTREACHED hit. Did not receive ping from zygote child`. **`zygote_fail` 110 -> 1.** **BOTH WAYS by `probe20`**: before, `clone(SIGCHLD|CLONE_NEWPID)` is the ONLY variant answering EPERM; after, `pid=4 ... code=7`; NEWNS/NEWNET/NEWUTS/NEWIPC/NEWCGROUP and `unshare(CLONE_NEWPID)` still EPERM. **`--uid 0` is NOT an alternative - chromium refuses root without `--no-sandbox`.**
- **CHROMIUM'S UNREACHABLE PADDING IS `IMMEDIATE_CRASH()` = `int3; ud2`** (`cc 0f 0b`, `base/immediate_crash.h`). int3 is TRAP-class so the reported rip is the NEXT byte (the ud2): **one crash rip therefore yields BOTH `Exception(3)` and `Exception(6)`**, not two crashes. So "the rip is in padding" means a NORETURN call returned OR a CHECK fired. **`file_offset = vaddr - 0x910001000`** for the exec PT_LOAD; slice with `dd` + `objdump -D -b binary -m i386:x86-64 --adjust-vma=<vaddr>` (seconds, vs minutes for objdump on the 327 MB binary).
- **GUEST FILES ARE NOT HOST-VISIBLE.** `/tmp/shot.png` written by a guest lives only inside litebox. **To prove a render, run `/usr/bin/python3 -u -c <script>` as the program** and let the GUEST decode its own PNG - `--initial-files` puts `usr/bin/python3` in the rootfs even though there is no `/bin/sh`.

Mechanism prose for every sha is in `docs/AGENTS_ARCHIVE_*` (newest `-08a` ... `-09-03`); this file keeps the RULE, the `file:line` and the proof.

- **`e4f749d` A LISTENING PORT BELONGS TO EVERY LIVE REFERENT, NOT TO THE PROCESS THAT ARMED IT** (`net.rs` `ListenQueue::ref_pids` + `release_listen_queue`): retire only when NO referent pid is live AND `refs` is spent; `reclaim_orphaned_listen_queues` (1/512 ticks) frees rows whose referents all died without closing. **BOTH WAYS by lqref1: new `CONNECT_OK body=b'CHILD'` x3 / `retire=false`; `LITEBOX_LISTEN_QUEUE_LEGACY_RETIRE=1` `CONNECT_FAIL` x3 / `retire=true`.** **`diag-listener` logs `refs`/`me`/`owner`/`owner_is_gone`/`live`/`retire` per close.** **A row with NO referent pid is NEVER retired (pid 0 = unknown, not dead).**
- **`eee620f` AN UNENUMERATED HOST REFUSAL IS AN ERRNO OR A SIGNAL, NEVER A PANIC**: console stdio failure + a dead console-pump thread = EIO/EOF (`devices.rs`); `exception_handler` maps GUARD_PAGE/STACK_OVERFLOW/IN_PAGE_ERROR -> SIGSEGV, SINGLE_STEP -> SIGTRAP, FLOAT_* -> SIGFPE, any other code = a throttled `error!` + SIGSEGV; `dup`'s unfilled slot = EMFILE. **DEFENSIVE - NOT PROVED BOTH WAYS.**
- **`61fb111` A FILE SIZE THIS STORE CANNOT BACK ANSWERS EFBIG, NEVER A PANIC** (an in-mem file's bytes are ONE contiguous host allocation). **RULE: a guest number that becomes an allocation size must be validated at the SYSCALL boundary - `try_reserve` alone does NOT help.** **BOTH WAYS by panicx1: before, the run dies at `memfd_big` (`WORKER_RC=137`); after `memfd_big`=EFBIG(27) + `pwrite_far`=EFBIG(27), 10/10 OK.**
- **`5dce756` A SECOND WAVE OF SYSARG PANIC SITES ANSWERS AN ERRNO, NEVER A PANIC**: `setsockopt` on the wrong socket type, a short AF_UNIX `addrlen`, a failing `accept`/`socketpair` close, a `ppoll` `nfds` from the guest, `clone(CLONE_SETTLS)` out of range, `execve` TLS clear, an exhausted `SharedUnixConnTable` arena. **panicx1: 10/10 OK (`keepintvl_udp` 92, `ppoll_bignfds` 22, `clone_badtls` 1).**
- **`0095f6f` AN UNSUPPORTED SOCKET TYPE/DOMAIN, `prctl`/`arch_prctl`/`fcntl` COMMAND OR ROBUST-LIST PID ANSWERS AN ERRNO, NEVER A PANIC**: INET `SOCK_SEQPACKET` = EPROTONOSUPPORT, a domain with no socket = EAFNOSUPPORT, `get_robust_list(<other pid>)` = EPERM, an unknown command = EINVAL, a PI futex = ENOSYS. **BOTH WAYS by sockx1: before `panicked at syscalls/net.rs:1345` + `rc=137`; after 10/10 OK.**
- **`8635867` A SYSCALL ON THE WRONG FD KIND ANSWERS AN ERRNO, NEVER A `todo!()` PANIC**: `ftruncate`/`fallocate` on a socket or pipe = EINVAL, `fcntl(F_SETFL)` on an epoll fd = accepted, an unhandled stdio ioctl = ENOTTY. **truncx1: `file rc=0 size=4`; pipe+socket `rc=-1 errno=22`.**
- **`4fbbf9e` A RENAME MOVES THE STORE'S ROW, NOT THIS PROCESS'S COPY OF THE FILE** (`move_slot` `file_spill.rs:173`). **BOTH WAYS by renamecoh1 - the writer is STILL RUNNING when the rename lands: `on` `moved=104 a=104`; `off` `moved=0` BAD.**
- **`8f33179` AN `O_APPEND` WRITE IS PLACED BY THE STORE, NOT BY THE WRITER'S OWN COPY** - `O_APPEND` must be recorded per fd (`fd_append`, set at open). **BOTH WAYS, ONE BINARY: `LITEBOX_SHARED_WRITE_PREFIXES=/tmp/co/` = `concurrent 400 (a=200 b=200)`; without it `concurrent 200 (a=0 b=200)` BAD.**
- **`7028ef5` TWO HOST PROCESSES WRITING ONE FILE CONVERGE THROUGH THE SHARED WRITE STORE.** `LITEBOX_SHARED_WRITE_PREFIXES` shares any host-named path on top of `SPILLED_PREFIXES`; `sync_spilled_fd` pulls the store's bytes in before EVERY read/write; `install_spilled_content` asks for bytes BEFORE truncating. **BOTH WAYS: `lockapp1 contend` `rows=500 by_who=[(1,100),(2,200),(3,200)]` vs `rows=300` BAD; `spillx1` `len=2000 A=1000 B=1000` vs `len=1000 B=0`.**
- **`722d8c0` A GUEST STARTS WITH ITS IMAGE'S OWN `Env`, AS `docker run` DOES.** `oci.rs` records `config.Env`; `LITEBOX_IMAGE_ENV_OFF=1` restores the empty env. **BOTH WAYS by exeprobe3: ON `ENV_PATH='/lsiopy/bin:...'`; OFF `ENV_PATH=None`.**
- **`08b1856` A GUEST-REACHABLE LENGTH OVERFLOW ANSWERS AN ERRNO, NEVER A PANIC.** `sys_madvise`/`sys_mprotect` round with `checked_next_multiple_of`. **mmapx1: `madvise(len=2^64-1)` = `rc=-1 errno=22`.**
- **`67b198f` A POSIX RECORD LOCK EXCLUDES ACROSS HOST PROCESSES.** `SharedRecordLockTable` (256 rows, arena, keyed `(dev,path)`, dead holders reclaimed by host-pid liveness); `LITEBOX_RECORD_LOCK_SHARED_OFF=1` restores the old path. **BOTH WAYS: reclock1 `CONTEND_FIRST=errno=11`/`WAIT_ELAPSED_MS=3873`; reclock1off `OK`/`0`.**
- **`7efefc5`** see Where things stand; it also instruments the inbound path: **`diag-pump`** (+`-write`/`-read`) with `state`/`recv_queue`/`send_queue`/`to_real`/`to_guest`/`real_eof`/`fin_sent`, BIDIRECTIONAL `diag-loop-inbound`, `diag-inbound-{accept,drop,listener}`. **`SocketSet` has NO `len()`** - `.iter().count()`. **`00d0c3c` A CROSS-PROCESS FORK CHILD EXPORTS WHAT IT CHANGED, NOT ITS WHOLE LAYER** - a child's export is a PATCH: byte-identical entries withheld, growth as a TAIL at the reader's own EOF, anything else as BLOCK RANGES; kind travels in ustar `gname` (`lbxmerge a <base>` / `lbxmerge r <offset> <final_len>`). **An unmarked payload still replaces the file.**
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

1. **LINUX CHROMIUM: NO RENDERER, `dump_dom_lines=0` (the GPU loop is FIXED - this is what is LEFT).** The browser runs ~71 s and forks 82 sandboxed children, then **`Zygote could not fork: process_type utility numfds 6 child_pid -1`** -> **`NOTREACHED hit. Did not receive ping from zygote child`** -> `exit_signal Signal(5)` at `rip=0xa192b194d` (a NEW site, NOT `0x9133314c2`). Two measured symptoms sit right there, neither root-caused: (a) **250** `recvmsg: rebuilding a carried SCM_RIGHTS fd failed errno=ENOENT spec=F|1|0|/tmp/udd-<tag>/Default/Session Storage/LOG` - a file the writer created but has NOT been REAPED (see below), reopened `O_WRONLY` with no `O_CREAT` in the receiver; (b) ~30 `spec=T|2|0|lbxpool:<addr>` zero-size snapshot carries also ENOENT (`rebuild_snapshot_file` -> `shared_memory_from_token`). Also visible: a ~35/s spawn-exit loop of `Chrome_ChildIOThread`/`PerformanceTraceTask` children from t≈68 s. **The `Zygote could not fork` message is NOT automatically the `2f6ff322` bug - that one is fixed (`zygote_fail` 110 -> 1); get the clone errno before re-deriving it.**
2. **CLOSED - NO LIVE SERVER'S PORT GOES DEAF** (`57b87eb`, chrF34; chrF35's "8081 went deaf" WAS HOST
   MEMORY - chrF36: 27/27 host probes `200 786`, row never retired; xproc41/42: busy 41/41, deaf = live
   owner that never accepts -> refuses at backlog 8 and RECOVERS, vs orphaned -> ~25 ms PERMANENTLY).
   **RULE: `selkpid=none` is a ps ARTIFACT; `slots=none` = no socket on that port at that instant.**
3. **CLOSED - chromium's kills are guest-side**: `Exception(14)`/`error_code=0x15` = an instruction fetch of
   a PRESENT non-executable page (`rip` in a `VM_*` range with NO `VM_EXEC`, `rax == rip`: it jumped to
   DATA); `Exception(3)`/Signal(5) with `cc 0f 0b` at `[rdi]` = chromium's OWN `IMMEDIATE_CRASH`, and it
   KEPT PAINTING; `mprotect(PROT_EXEC)` DOES LAND. **A 0-BYTE UNIX READ MEANS `peer_gone()`, NOT A CLOSED
   PEER** (`unix.rs`). `--initial-client-fd=FD`; `--database=PATH` is REQUIRED. (Prose in `-07q`.)
4. **BYTE STORE: CLOSED FOR BOTH REAL CONSUMERS, FOR RENAME, AND FOR THE NON-APPEND `pwrite` SHAPE**
   (numbers under `7028ef5`/`8f33179`, all BOTH WAYS). **What is left is architectural**:
   `install_spilled_content` is a whole-file `O_TRUNC` replace on first publish, `self.locked(..)` is held
   across `platform.spill_write` (host IO under a spin lock), and `FileX { data: Cow<'static,[u8]> }`
   (`litebox/src/fs/in_mem.rs:1530`) is PER-PROCESS - bytes cross only at fork spawn/exit EXCEPT through
   `SharedFileSpill` (`file_spill.rs`). Also open: AF_UNIX exhaustion is silent (256 slots, keys >108
   bytes); `timerfd`/`signalfd` uncarriable; **cross-process-fork state NOT restored**.
5. **THE ARENA: one `create` costs ~48.5 MiB**, so only 2 fit the 128 MiB Windows arena and
   `create_shared_kernel_state` PANICS there; an `attach` costs 0 (1 create / 503 attach). Make `create`
   idempotent by slot, only if creates exceed 2. **The LINUX arena is already RECYCLED.**

## How to run and drive it (full prose: `docs/HARNESS-LESSONS.md`)
Full prose in `docs/HARNESS-LESSONS.md` (verbatim: `-07q`). This section is the WINDOWS harness
(`.wfgy/guest2.ps1`, PowerShell, `Stop-Litebox`); nothing in it applies to the Linux container. Transferable
rules: sweep runners before a run and never run two runs at once; a gate is a SNAPSHOT, not a reservation;
`.err` SIZE = run health; bound every wait (~20 s) or "never started"/"stuck" are indistinguishable;
`rc=137` at the END of a run is the harness cap, not a bug; a guest `python3` must be run `-u`; **a probe
that cannot fail is not evidence** (a control that must succeed plus an INVALID guard); **prove BOTH WAYS**.

**Linux chromium harness: `/tmp/lbx/chrdom.sh <tag> <secs>`** (`--headless=old --disable-gpu --virtual-time-budget=8000 file:///page.html`, `GETFLOFF=1` sets `LITEBOX_SHM_CARRY_GETFL_OFF=1`). It prints `dump_dom_lines`, the `exception=Exception(n)` histogram, `zygote_fail`, `init_sandbox`; `rc=124` = the `timeout` cap. `/tmp/lbx/chrshot.sh` is the screenshot variant; judge a PNG with `python3 /tmp/tools/pxstat.py`.

## Standing lessons and hard constraints (mechanism: `docs/HARNESS-LESSONS.md`)

- **`litebox::sync::RwLock::try_read`/`try_write` FAIL WHEN A WRITER IS MERELY QUEUED** (`litebox/src/sync/rwlock.rs`): ONE thread parked in a blocking `descriptor_table_mut()` makes every `try_descriptor_table()` fail forever and the per-tick socket sweep a silent no-op in that process. Dead-holder recovery needs the owner thread DEAD; a parked one is never recovered.
- **THE CROSS-PROCESS FORK'S COPY PLAN IS GRANULE-WIDENED AND MERGED** (`process.rs`: `GRANULE = 0x1_0000`; a group base MUST stay granule-aligned or Windows answers 87). **NOTHING IN `litebox/src/mm` ZEROES A FRESH PAGE** - the guarantee is MAP_FRESH. **`vma_layout()` zips `ranges`/`flags`/`executable`/`is_file_backed` POSITIONALLY - filter ALL or NONE.**
- **A GUEST PACKET TO `127.0.0.0/8` OR TO `GUEST_IP_ADDR` IS LOOPED IN-PROCESS** (`phy.rs:166`): a guest's `127.0.0.1:<port>` NEVER reaches the host's published listener. **`data_granted` = CAPACITY granted, `data_used` = slots in use**.
- Guest-reachable code returns an errno, never a panic (the host process IS the whole session). Refusal errno is contract: EPERM degrades, EINVAL/ENOSYS fails hard. `LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check. Never subtract timestamps across a parent and a fork-child log.
- **A `TypedFd` index is valid only against the `Descriptors` that inserted it, and only for its own subsystem.** **A `SharedUnixAddrPresenceTable`-shaped table needs every write path mirrored, and no per-tick sweep may DROP another process's object.** **A backlog slot of a listening port is reachable by NOTHING but `accept`.** **`(dev,ino)` is cross-process-stable for IMAGE files ONLY** - key shared registries on `(dev,path)`.
- **Host memory: check host RAM FIRST.** gm's browser (2.2 GB PER INSTANCE), the `queue.mjs` chrome storm and `bun` - **none of these is mine to kill. When free RAM is short there is nothing safe to reclaim - wait.**
- **A process's writes are invisible to everyone until it is REAPED, not when it closes**; a fork child's writes reach the parent only on `wait4`. Cross-process fork carries pipes/regular files/eventfds/pty/unix sockets + INET + `MAP_SHARED`, drops pty fds and non-INET cloexec fds; slots (6) gate the spawn, fail open after 8 s.
- Logs: verbosity from `LITEBOX_LOG`, not `RUST_LOG`; **`debug!` fields take `&str`, not an owned `String`**. **Timestamps are PER-PROCESS UPTIME; `.err` lines carry ANSI escapes.** **The `tag=`/`dtag=` fields DO NOT identify a host process.** Socket census is `local_port:state:remote_port`; a listening slot is `0:L:0`, so the census CANNOT prove a port is armed. **`LITEBOX_*` env flags are read from the HOST env by `platform.env_flag`, not from `--env`.**

## Guest stack: chromium + selkies (full recipe in the `-06f` appendix, verbatim in `-07m`)
Windows/webtop recipe, verbatim in the `-06f` appendix and `-07q`. Two chromium failure modes that
must not be merged: (a) `Crashing due to FD ownership violation:` + `No usable sandbox!` = the
`CanCreateProcessInNewUserNS()` probe; (b) rc=133 (SIGTRAP) with NO sandbox line = crashpad. The
proof is CDP from inside the guest PLUS the host browser; judge a frame only after ~60 s of STREAM
time. Selkies binds `--port=` and needs `--enable-basic-auth=false`; it EXITS when its last client
leaves, so a published port going `000` is the APP LEAVING.

## Closed - do not re-attempt without a genuinely new approach

**REFUTED ON THE LINUX HOST (chromium), each by a probe that could have failed - do not re-open without a NEW reproduction:**
(a) **"abort/SIGABRT is broken"** - `probe18`/`19`: `abort`/`raise`/`tgkill`/`tkill`/`kill(self,SIGABRT)` and a real `-fstack-protector-all` smash ALL die on Signal(6), single- and multi-threaded; `smash` prints `*** stack smashing detected ***`.
(b) **"the guest FS base moves, so `%fs:0x28` misreads"** - `probe24`: 200k main + 100k pthread + 200k canary frames: **0 changes**. **`diag-fsbase` (`litebox_shim_linux/src/lib.rs:2419`) is UNRELIABLE**: it reads the HOST `.tbss` slot while the platform SWAPS fs/gs, so its "moved" lines are an artifact no guest instruction observes.
(c) **"seccomp traps abort's syscalls"** - all 511 `PR_SET_SECCOMP` calls are `prog=0x0` probes -> EFAULT, **ZERO** `delivering SIGSYS` lines.
(d) **"per-pid `/proc` is missing"** - `probe22`: `/proc/<pid>/{stat,status,comm,task/<tid>/status}` and `ls /proc` are correct for a live fork child; the ENOENTs are children that ALREADY exited.
(e) **"the frame is corrupt / `rsp` is wrong"** - `rsp = rbp-0x260` is exactly right; all 5 backtrace return addresses are each preceded by a `call`. (f) **"litebox hooks `__stack_chk_fail`"** - codesearch finds no such hook.
(g) **"the `/proc/self/fd/N` O_RDONLY reopen loses the access mode"** - `probe27`: `plain` and `cloexec` arms both `MODE=0` with `SAME=OK`, `guard` arm `ERRNO(2)`. The reopen was always right; only the `S|` carry spec was.

Verbatim in `docs/AGENTS_ARCHIVE_2026-10-07k.md`. **REFUTED for 8081: `xproc18`/`19`/`20`; "selkies forks a child holding the descriptor table"; "a busy port goes deaf" (41/41); "an attached published host client deafens the port to the guest" (37/37).** **CLOSED: `eb16abf`, `150e6e0`, chrF14 window loss, apps10 `BadMatch`, chrF24-27, grey/`NO_PNG`, xproc44, the 23-arm app census.**

## Docs and tooling map
Archives under `docs/`: `-08a` newest ... `-09-03` oldest (`-05b` S10 = runner/OCI + child
exit, `-06f` = the closed/appendix); `docs/HARNESS-LESSONS.md` = harness prose,
`docs/LINUX-TEST-SUITE.md` = Linux-container notes. `.wfgy/` is git-IGNORED (gm DOES scan it
when you pass `path`). gm `codesearch` (INVARIANT 4: never grep/find): `literal` is
exhaustive, `dual` a ranked SAMPLE - never conclude "absent" from it; scope with `path`/`glob`
(unscoped literal caps at 40 matches); **dual mode SILENTLY IGNORES `path`**. gm DOES scan a
git-ignored dir when you pass `path`; a huge log is skipped by a 16 MiB ceiling, not binary
sniffing - `fs_read` pages it. Spool fallback: fields are FLAT
(`query`/`mode`/`path`/`output`/`cwd`/`session_id`), never nested under `body`; write
`in/<verb>/<session>-<N>.txt` atomically (temp file + `mv`), read `out/<verb>-<session>-<N>.json`.
