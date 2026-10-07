# AGENTS_ARCHIVE_2026-10-05g -- the dead published port: the listening backlog

Section 1 is the verbatim `AGENTS.md` as it stood at the start of this pass (`-05f`, 29,998 bytes),
kept so nothing has to be recovered from git history. Section 2 is this pass's mechanism: the
root cause of Open #1, what was refuted on the way, the fix (`39616f4`) and its verification.

---

## 1. Verbatim AGENTS.md (2026-10-05f)

# litebox -- current state (2026-10-05f; recompact of `-05e`; verbatim `-05e` in `docs/AGENTS_ARCHIVE_2026-10-05f.md` intro, this pass's mechanism in that file)

CURRENT-STATE index. Every claim carries a sha, `file:line`, or numbers. Shorthand: `process.rs`/`file.rs`/`mm.rs`/`unix.rs`/`net.rs` = `litebox_shim_linux/src/syscalls/<x>`; `platform/lib.rs` = `litebox_platform_windows_userland/src/lib.rs`; `fork.rs` = `.../process_fork.rs`; `fd.rs` = `litebox/src/fd/mod.rs`; `lazy.rs` = `.../lazy_file_map.rs`; `mm/linux.rs` = `litebox/src/mm/linux.rs`. **This file wins.**

## Where things stand

- **GOAL MET AND RE-MEASURED on `4964934` (chrD92)**: sandboxed chromium -- its OWN sandbox, no `--no-sandbox` -- renders VISIBLE IN A REAL HOST BROWSER (`chrdesk54.sh` + `-Publish 8081:8081`). Host-browser frame `1354x834 readyState=4 blue=42.41% root[48,80,112]=53.91% white=0.00% lightGrey=0.00%`; guest xwd `blue=99.79%`; CDP `shot 800x600 blue=99.78%`, `bg rgb(32,192,240)`; **ZERO `panicked at`** over 1.5 MB of `.err`. chrD90 (`47cefb2`): `1354x832 blue=42.22%/root=54.11%/grey=0/white=0`.
- **HEADLESS TOO (cb66 arm Z, `f345821`)**: DEFAULT config -- zygote ON, chromium's OWN sandbox, GPU process FORKED from the zygote -- `PNG_APPEARED i=6`, `800x600 #20c0f0=99.95%`, `CHROMIUM_RC=0`. `--in-process-gpu` is only the control now.
- **APPS RE-VERIFIED on `4964934`** (apps1d/apps3d, zero panics): `xterm` red `255,0,0` 97.4%, `xclock` blue 95.1%, `xcalc` white 83.2%, `xeyes` 155 colours, `xedit` white 94.2%, chromium `--app` `240,192,32 99.8%`, `mousepad` white 87.4%, `thunar` white 60.9%. **Open #2 CLOSED.**
- **Branches**: `inetfix` == `4964934` on top of `37c2374`/`7c1d987`; **local `main` is still at `47cefb2` -- fast-forward pending, never done automatically.**
- **THE SELKIES DEATH IS `47cefb2`, NOT HOST RAM**: `DescriptorEntry::as_subsystem_mut()` (`fd.rs:1095`) did `downcast_mut().unwrap()` and panicked the whole guest process (101 -> rc 137) at selkies' first post-connect fork. The chrD79-85 "ENVIRONMENTAL, host RAM" verdict is WITHDRAWN. Last line `Could not obtain XFCE session environment` = the fork child died (survivable -- chrD90 `RC: 1`); host-RAM starvation is separate. **Check free RAM first regardless.**
- Judge a host-browser frame only after ~60s of STREAM time (chrD32 `white=42.61%` -> `blue=42.23%` at t=61s); white/grey is PRE-FIRST-PAINT. Guest time dilates 3-4.6x vs wall; attach a host browser only to take the frame.
- **Before ANY run**: sweep runners by CIM (`Get-CimInstance Win32_Process -Filter "Name like 'agentplug%' OR Name like 'litebox%'"`), close other host browsers, require >=2.9GB free. `taskkill /F /T` did NOT kill a 1013MB runner; `Stop-Process -Force -Id` did (some are undeletable but killable). GM's own `agentplug-runner.exe daemon` holds ~1.2 GB -- leave it. Sweeping 52 leaked runners took free RAM 2596 -> 3573 MB.

## Fixed, newest first (mechanism per sha in `docs/AGENTS_ARCHIVE_*`; keep only the RULE)

- **A LISTENING PORT MUST RE-ARM ITSELF FROM THE TICK, NOT ONLY FROM `accept` (uncommitted, this pass; closes Open #1).** `TcpServerSpecific` holds one smoltcp listening socket PER backlog slot (`backlog` clamped to 8 in `listen`); a slot can vanish (`reset_after_poisoning()` wiping the shared `SocketSet` leaves a descriptor's handles dangling). With every slot gone the port has no listening socket, no SYN can make a slot `Established`, so `drain_socket_channel_buffers`'s readable re-arm never fires, so the server never calls `accept`, so nothing re-arms: **the port is dead for the rest of the session while its already-accepted connections keep working** (chrD92: in-guest `curl 127.0.0.1:8081` refused from t=120 on, selkies' websocket still streaming). Two fixes in `net.rs`: `accept`'s `NoConnectionsReady` arm now refills when its `retain` dropped a slot (`diag-accept` warn), and `drain_all_socket_channel_buffers` runs a second `iter_mut_nowait` pass, `repair_listening_backlog`, that re-arms ANY listening socket short of its backlog -- including one whose every slot went stale (which never reaches `accept`) and one a table-full refill left short (`refill_to_backlog` stops at `MAX_SOCKETS`=256 and never retried; its `"socket table is full"` warn has ZERO hits in chrD92, so that was not chrD92's path). The sweep skips `consider_closed` descriptors, never removes a socket still in the set (cross-process heap), and stays silent unless a port went from armed to zero live slots. Host side proven innocent: pub5 29/29 host connects with no guest server; pub4's host refusals were the probe outliving the runner (401s host vs ~230s guest), guest-side 45/45 200s.
- **`4964934` A CLOSE OF A NUMBER NOTHING OWNS IS EBADF, NEVER A PANIC.** chrD91 died on `unreachable` at `fd.rs:141`, preceded by `epoll poll with socket fd: EBADF` (`epoll` stores `TypedFd`s and holds no file reference): an owned, unclosed `TypedFd` whose slot is ALREADY EMPTY. Same treatment for `Descriptors::remove` (`fd.rs:114`), `Network::close`'s Deferred arm (`net.rs:1794`), and `do_close_and_replace`'s fallthrough (`file.rs:4679`). **reg1e byte-identical to reg1d (46 PASS / 0 FAIL); chrD92 zero panics.**
- **A CARRIED SHARED REGION MUST REACH THE CHILD WITH ITS OWN PROTECTION, NOT THE WIDEST (`37c2374`).** The fork parent mapped every carried region at the widest protection the section allows and `adopt_carried_shared` only bookkeeps, so a guest `PROT_READ` `MAP_SHARED` region arrived `PAGE_EXECUTE_READWRITE`. Fix: `VirtualProtectEx` down to `prot_flags(carry.perms)` while the child is suspended; a narrow can still fail on the same ceiling, so failure is reported, not fatal.
- **A PRIVATE FILE MAPPING IS A COPY-ON-WRITE VIEW OF ONE SHARED SECTION (`7c1d987`).** `VmFlags::VM_PRIVATE_FILE_COW` (bit 11) -> `MemoryRegionPermissions::COPY_ON_WRITE` -> `PAGE_WRITECOPY`/`PAGE_EXECUTE_WRITECOPY`, `MAP_PRIVATE` not `MAP_SHARED`. So `mprotect(PROT_READ|PROT_WRITE)` on a `PROT_READ` mapping SUCCEEDS (Linux grants `VM_MAYWRITE` at mmap time), the write is this process's own, and the 324 MB binary stays ONE copy across ten processes. **cb68: F4/F4B/F4C/F4D/P2 `rc=0 errno=0`, write sticks parent AND child; CTL1 (unmapped) still ENOMEM=12. cb70: privacy holds both directions across a cross-process fork, `F_FILE_UNMODIFIED=True`.** RULE: **the qualifier travels with the VMA** -- `insert_mapping`, `protect_mapping`, `duplicate`, `shared_region_carry` OR it in, and the fork PARENT's `MapViewOfFile3` must ask `PAGE_*_WRITECOPY` (else the child's writes land in the object every mapper sees: cb70 `P_SEES_FILE_NOT_CHILD_WRITE=False`).
- **A `PROT_NONE` FILE MAPPING IS A RESERVATION, NOT A SHARED READ-ONLY VIEW.** The shared read-only path carried flags **89** (no `VM_MAYWRITE`), so a later `mprotect(PROT_READ|PROT_WRITE)` got **EACCES** in the parent AND a cross-process child (cb45 F2). Fix: exclude `prot.is_empty()` (**cb45b `rc=0 errno=0`**). `a2ac697` a `PROT_NONE` file mapping must still carry the file's bytes.
- **`reset_pages` no longer `unimplemented!()`s on `madvise(MADV_DONTNEED)` over a file-backed range** (a panic there kills the session): a no-op preserving the bytes -- legal, since `MADV_DONTNEED` leaves contents UNSPECIFIED.
- **`f345821` AN INODE NUMBER MUST NAME THE FILE, NOT COUNT THE PATHS THIS PROCESS HAS STAT'ED.** A cross-process fork child rebuilt its fs with an EMPTY lookup table, handing out 1,2,3... in touch order, and **ld.so decides "already loaded" by `(st_dev, st_ino)` vs the PARENT's numbers** -- cb64 watched a byte-identical COPY of libEGL.so.1 come back AS libm, every `dlsym` NULL, GPU process dead on `call *rax` with `rax=0`. Fix: `layered_ino()` = FNV-1a over `(dev, ino, rdev)`, forced odd. **cb65: all 10 child dlopens name themselves, `IDENT_CHANGED_VS_PARENT=0 of 69`.** RULE: **any number a guest can observe across processes must be a function of the file, never of this process's history.**
- **A CARRIED SOCKET'S RX MUST REACH THE PROCESS THAT IS READING IT.** A carried socket has TWO proxies (one per process heap) and every tick drained into whichever ran first. BOTH halves were needed: (1) the shared tick skips the TCP **and** UDP rx drain when `shared && !proxy.has_observers()`; (2) `receive` (`net.rs`) calls `drain_rx_into_proxy(fd)` first when shared -- a NON-BLOCKING read registers no observer, so (1) alone starves it. **12/12 blocking + 6/6 non-blocking clean; `reg1.sh` all PASS.** Residual: both referents observing still race.
- **`47cefb2` A MISTYPED FD IS EBADF, NEVER A PANIC.** Every `TypedFd` resolver in `fd.rs` gates on `matches_subsystem::<Subsystem>()`; `with_metadata` (`fd.rs:596`) and `get_proxy` (`net.rs:1194`) too. `4964934` extends it to the close path.
- Older, still binding: `9997313` INET crosses a cross-process fork, and **FD_CLOEXEC is about `execve()`, not `fork()`** (rejections log at `warn!`, so a dropped INET fd leaves ZERO evidence at default log); `a629714` a fork child SHARES `MAP_SHARED` -- **validate every segment BEFORE reserving the group placeholder**; `6183f53` an unplaceable page-granular hole RELOCATES a `Hint`; `f260226` cross-process `flock(2)` exclusion, keyed `(dev, path)` NOT `(dev, ino)`; `6a472b5` `shmat` on an `IPC_RMID`'d-but-still-attached segment SUCCEEDS (X11Libre's BadAccess there killed every GTK app); `f5d73ff` the flock registry is per host process -- **that panic correlated with LAUNCH ORDINAL, invalidating every A/B in that window**; `575f0e2` an uncarriable fd silently drops the fork off the cross-process path, so **never run the stack with fork OFF**; `aca3a53`/`9f7c994` one store per object / sized memfd; `ae6926d` (branch `drmevdev`, unmerged) shared DRM/evdev tables.
- Pipes/locks/waiters: `31ca26b` `MAX_INLINE_WAITERS` 32->128; `1732ad9` a carried-pipe pump must not outlive its child; `463dc29` **never drain a pipe the guest holds for itself** (libuv's GLOBAL SIGNAL LOCK); `16f3e76` **no path holding `net_lock` may block**; `a1423ee` HOLDING is not READING; `7d2a6a7` `SHARED_UNIX_CONN_CAPACITY` 1024->4096 -- chromium renders with its own sandbox; `efce1a5` `/proc` enumerates the whole session (`guest_pids=30`; before **1**).

## Open, in rough priority order

1. **VALIDATE THE LISTENER RE-ARM (chrD93).** Fix committed, binary rebuilt; a full stack run must show in-guest `curl 127.0.0.1:8081` answering 200 through t=420 with `diag-accept`/`diag-listener` absent or recovering. If it still refuses, census the 256-socket table -- the publish path is proven innocent (pub1/pub5).
2. **chrD83: 2 x `Exception(14) error_code=0x15`** = user-mode instruction fetch to a PRESENT non-executable page, distinct from the `0x7` relocating-fork signature. chrD79/85 have ZERO and chrD83 ran starved beside 16 `chrome.exe`; **re-measure on a clean host before calling it a litebox bug.** `LITEBOX_DIAG_FAULT_VQ=1` shows `PAGE_READONLY` where `PAGE_EXECUTE_READ` is expected; `prot_flags` (`platform/lib.rs`) is CORRECT. `cb9.sh` (map in parent, execute in child) and `cb19.sh` (2 generations) never run.
3. A real `flock(2)` consumer (chromium's profile lock, SQLite) -- `f260226` is verified by `.wfgy/flockx1.sh` only.
4. Host memory is the binding constraint, not a litebox bug. **Check host RAM FIRST.** `LITEBOX_DIAG_ALLOC_STACK=1`, `_MEM_BREAKDOWN=1`; `memsamp.ps1`, `wsmap.ps1 -ProcId`. **Snapshot-binary runners leak: sweep by CIM, not by name.**
5. **The 128MiB shared kernel arena is nearly full from ~6 concurrent `GlobalState`s** (each 15-21MB, dominated by `SharedUnixConnTable`): `arena exhausted` hits ~1 run in 5 at BASELINE -- size any new arena-resident table against that.
6. Verify in the full stack (committed, never seen in a browser): `3e1ef47` SCM_RIGHTS over a cross-process unix connection; `b012910` lazy file map stale entries; `08ae94f` `/proc/<pid>/fd`. Repro `.wfgy/th1.ps1 -Run th1|th2|th3`.
7. Native kernel-COW fork on Windows (`.gm/prd.yml` `native-kernel-cow-fork`); writable layer shared across processes; AF_UNIX exhaustion still silent (256 slots, keys >108 bytes); `timerfd`/`signalfd` uncarriable.
8. Chromium startup flakiness: chrD56 rc=0 with `crashpad ... mkdir .../Crash Reports: No such file or directory (2)`, never mapped a window -- `homemk.sh`: a parent-made dir chain IS visible to a `setpriv --reuid 911` child 30/30, so timing, not visibility.
9. **Fast-forward local `main` to `inetfix`** (`main` is at `47cefb2`). Only ever push to `origin` with per-instance confirmation.

## How to run and drive it (harness lessons that cost sessions)

- **No WSL, no hypervisor, ever.** Native Win32 exe; host tooling PowerShell or Git Bash; launches `CREATE_NO_WINDOW`.
- Cheap repro: `target/release/litebox_runner_linux_on_windows_userland.exe -Z --oci-image docker.io/library/debian:stable-slim -- /bin/bash -c '<script>'`. PowerShell never Git Bash for guest paths; single quotes only inside `-c`. **Build ONLY `cargo build --release -p litebox_runner_linux_on_windows_userland`. Never pipe a build through `tail`.**
- **The OCI layer cache is `./.litebox-cache` at the repo ROOT (24 GB) -- `target/release/.litebox-cache` holds only `boot.lock`.** Keys are versioned (`_v2.tar`); after a format bump every layer logs `[cache] MISS ... (v2)` and the run re-pulls the whole image (~30-60 min at ~520 KB/s) before the guest starts -- check `.err` for `MISS` before calling a run hung (chrD93: 0 bytes of `.out` for 30 min = cold cache). **Watch disk** (18 GB free with a 24 GB cache).
- Full stack: `.wfgy/pass118_full.ps1 -Run <name> -MaxSeconds N`. Cheap repro: `.wfgy/guest2.ps1 -Script <sh> -Run <name> -Secs <n>` (script on STDIN to a guest `bash -s`); sets `LITEBOX_PROCESS_FORK=1`, `_LAZY_FILE_MAP=1`, `_OCI_USE_LAST_RESOLVED=1`; `-ExtraEnv`, `-Publish`, `-Exe`. **`rc=137` at the END of a run is the harness cap, not a bug.** A background launch reporting "exit code 128" is usually a lie -- `.wfgy/<run>.out` is authoritative.
- **Chromium does NOT exit after a failed `--screenshot`**: `wait` blocks to the harness cap and STARVES later arms. **KILL the arm (`kill -9 $ARM; pkill -9 -x chromium`)** and keep a capture's deadline BELOW the PNG poll window. **`timeout(1)` HANGS in-guest -- it is NOT a bound**; bound with `-Secs` or an in-guest background+wait loop, and never conclude "X hangs" from a cap-killed run without per-step BEFORE/AFTER prints.
- **`$?` after a pipeline is the LAST command's status**; **an unquoted heredoc expands `${PIPESTATUS[0]}`/`$(date +%s)` at FILE-WRITE time**; **a nested heredoc terminator hangs the script -- write probe files with the Write tool**; **Bash heredocs collapse `\\` -> `\`**, build Python escapes with `chr(92)`. **Guest scripts must be LF** -- count CR bytes in python, NOT `grep -c $'\r'` (false 187 for a file with 0 CR). **A probe that cannot fail is not evidence** -- include a negative control. `socket.timeout` has `errno=None`, so `"errno=%d"` raises TypeError and the traceback REPLACES the verdict (use `%s`).
- Cargo cannot relink while a runner holds the exe (`os error 5`): kill every runner first; if the lock persists **rename** the exe, build, delete `.prev*`. **Check the binary is NEWER than the commit under test.** In PowerShell `$?` is False after a successful `cargo build` whose stderr was redirected -- read the log's last line. Unkillable runner holding the boot lock: `Remove-Item -Force target/release/.litebox-cache/boot.lock`.
- `rc=137` from a fork child does NOT prove a kill: `decode_cross_process_exit_status` (`process.rs:390-403`) falls back to SIGKILL for ANY exit with no marker. `.err.log` SIZE is run health; `chrD*.err.log` is BINARY to gm `codesearch` -- use `.wfgy/logscan.py <file> <term>`. **Never dump a raw image into the transcript** -- compute the census in-page/in-guest.
- **Guest probe constraints**: there is **NO C compiler in the image** (`gcc` -> rc 127), so every probe must be python; `dlopen("/usr/lib/chromium/chromium")` fails anyway. **`python3` in a `guest2.ps1` run is itself a cross-process fork child** (`pid=3 ppid=1`). `readelf --dyn-syms` TRUNCATES long names and versioned symbols carry `@`; use `readelf -W`, strip `@`, skip `[`. **`LITEBOX_*` are host-side runner vars -- invisible in-guest**, so in-guest A/B of them is impossible. **`/proc/net/tcp` DOES NOT EXIST** (pub4 probe: `FileNotFoundError(2)`).
- Diagnostics: `cdb -pv -p <pid> -xd av -xd sse -c "~*kb 25; qd"` (NOT on PATH); `litebox_diag::{process_timeline,socket_read,unix_conn_teardown,stderr_capture}=debug`. The image's Xvfb is XLibre and ABORTS when `/tmp/.X11-unix` exists with the wrong mode -- `mkdir -p /tmp/.X11-unix && chmod 1777` BEFORE Xvfb; dbus-daemon non-forking; use `xfce4-session`. **A pipe the guest holds for itself must be probed with a zero-timeout `select()`, never `read`**; asyncio closes a child stdin the instant it exits -- keep `p.stdin`.
- Repo hygiene: tars, frame dumps, debug logs never in git (`.wfgy/` ignored); **commit as lanmower only, no `Co-Authored-By`**; **never `git push` to `origin` (github.com/AnEntrypoint/litebox) without per-instance confirmation.** `4792fc5` pins `*.rs`/`*.toml`/`*.md`/`*.lock` to LF -- a wholesale CRLF flip blew `git diff` up to ~27k lines.

## Standing lessons and hard constraints

- Guest-reachable code returns an errno, never a panic (the host process IS the whole session). Refusal errno is contract: EPERM degrades, EINVAL/ENOSYS fails hard.
- `LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check. Never subtract timestamps across a parent and a fork-child log (`init_logging()` resets elapsed time per child).
- **A `TypedFd` index is valid only against the `Descriptors` that inserted it, and only for its own subsystem** (`fd.rs:1095`, `net.rs:1194`). Shared-memory structs must hold no process-relative pointers. **A per-process object (a socket proxy) cannot be the delivery target for another process's data.** A `SharedUnixAddrPresenceTable`-shaped mutable table needs every write path mirrored to the shared side. Any per-tick sweep over shared state must not DROP an object another process allocated (wrong heap; see `remove_dead_sockets`).
- **`(dev, ino)` is cross-process-stable for IMAGE files ONLY (`f345821`)**, never for runtime-created ones. **Keep keying shared registries on `(dev, path)` (`FilesState::lookup_fd_path`)** -- `f260226`'s flock table depends on it. cb69: `IMAGE_MATCH=YES`, runtime `RUNTIME_MATCH=0 of 4` but `CHILD_COLLISIONS=0 of 9` -- a wrong number is silent, never a crash.
- Cross-process fork (`LITEBOX_PROCESS_FORK=1`): carried = pipes/regular files/eventfds/pty/unix sockets + INET (`9997313`) + `MAP_SHARED` (`a629714`); dropped = pty fds and non-INET cloexec fds. Slots (`CROSS_PROCESS_FORK_SLOT_COUNT` 6) gate the spawn and fail open after 8s; keyed by child host pid, freed at STARTUP-COMPLETE not at reap (`a24ce9d`). The env var is PRESENCE-CHECKED (`var_os`): `=0` still ENABLES it. The post-duplication clone site is SUPERSEDED -- believe `try_cross_process_fork`'s log.
- **Lazy file map**: chunks >= `MIN_LAZY_LEN` (256 KiB) are armed `PAGE_NOACCESS` and VEH-filled from a `'static` source slice keyed by OCI LAYER INDEX (`register_layer_sources`), so sources are identical in every process of a fork tree. **A guest reads guest strings through a bare host-side dereference with NO validation** -- `Some("")` means a real 0x00 byte, `None` means the read faulted.
- **`ps -eo pid,args` is INVALID for chromium's children -- every chromium process reports `--type=zygote`**; `/proc/<pid>/cmdline` and `comm` are EMPTY; `/proc/self/maps` can be EMPTY in a fork child (cb36: parent 108 lines, child 0) while `/proc/self/exe` works. Judge liveness by CDP or pixels regardless. **This is also why selkies' `_pids_of("xfce4-session")` always falls back.**
- `chroot(2)` is real (`e608959`): `FsState.root` is shared by `CLONE_FS`; `cwd` is root-space, dirfd-relative paths stay unrooted. Do NOT test `CLONE_FS` with a raw `clone(...)` from CPython (dies 139); use pthreads.
- Logs: default `warn,...fork_verify=error`. Verbosity from `LITEBOX_LOG`, not `RUST_LOG`. `litebox_util_log::warn!` takes a single literal with inline captures -- **no trailing format args, no `\`-continuation inside the literal**. `LITEBOX_PUBLISH` bind/parse results log at `info!` -- invisible at default level.
- Env: `LITEBOX_PROCESS_FORK`, `_LAZY_FILE_MAP`, `_IDLE_TRIM=0`, `_OCI_USE_LAST_RESOLVED`, `_DUMP_FRAMES`, `_PROCESS_FORK_IGNORE_FDS`, `_DIAG_FORK_SHARED_FORCE_FAIL`, `_FLOCK_SHARED_OFF`; diagnostics `LITEBOX_DIAG_LOCKSTALL`, `_WAIT_DUR`, `_LOCKSTALL_TRACE`, `_ALLOC_STACK`, `_MEM_BREAKDOWN`, `_FAULT`, `_WATCHDOG`, `_NO_FAULT_WATCHDOG`, `_NO_EXTERNAL_FAULT_WATCHDOG`, `_SOCKET_READ_TARGET`, `_FAULT_VQ`.
- Subagents: Sonnet, not Opus. Web search: Google, not DDG (camoufox if blocked).

## Child exit: how a parent learns (and how it can fail)

`arm_cross_process_exit_notifier` blocks a host thread on the child's INITIATING THREAD handle, pushes SIGCHLD into `process.shared_pending` + `interrupt_all_threads()`; `sys_wait4` re-polls every 15ms and runs `import_cross_process_writable_layer` on reap. **`waitpid(-1)` only sees entries registered in THIS host process -> ECHILD.** **SIGCHLD is dispatched only when the thread passes `check_for_interrupt`/`prepare_to_run_guest` (`wait.rs:41-64`)**, so glib-style SIGCHLD reaping can miss it; `signalfd` never wakes (`signalfd.rs:185-191`); `pidfd_open` (`inotify.rs:353`) does. **The external fault watchdog `TerminateProcess`es a process whose CPU delta stayed <=10ms for 15s** (armed only by `mark_fault_terminate_armed()`, `platform/lib.rs:2338`/`:2642`) -- NOT the selkies killer.

## Guest stack: chromium + selkies

- **TWO chromium failure modes -- do not merge.** (a) `Crashing due to FD ownership violation:` + `zygote_host_impl_linux.cc:129] No usable sandbox!` = the `CanCreateProcessInNewUserNS()` probe. (b) rc=133 (SIGTRAP) with NO sandbox line = crashpad/HOME.
- **A `HOME` chromium shares with anything root ran kills it** (a root-run app leaves `$HOME/.config` 0755 root -> uid 911 gets `stat .../Crash Reports: Permission denied (13)`; crashpad mkdirs only its LAST component). Fix: `mkdir -p $HOME/.config/chromium; chmod -R 777; chown -R 911:911` on a HOME nothing else touched -- `chmod 777` on a SHARED HOME is NOT enough.
- NON-ROOT (`setpriv --reuid 911 --regid 911 --init-groups`); `--user-data-dir` `chmod 777` when a root shell created it (else `SingletonLock: Permission denied`, exit 21). `HOME=/tmp/cuhome` exits 0 (`cpad1.sh`).
- `seccomp(2)` is real: `syscalls/seccomp.rs` interprets classic BPF at the top of `Task::do_syscall`; `SECCOMP_RET_TRAP` must RETURN the syscall number (`ad2659f`), payload in `si_errno` (`067367b`).
- Still open: (1) the launcher thread SERIALISES cross-process spawns, so children hit the 15s "no connection" self-termination; (2) `Vmem::duplicate` skips `VM_OWN_FORK_PADDING`; (3) ~1/run `rebuilding a carried SCM_RIGHTS fd failed errno=ENOENT`; (4) two simultaneous chromiums are unaffordable -- run ONE.
- **CDP from inside the guest is the decisive chromium probe** (`chrdesk38.sh`, `/tmp/cdp.py`): `GET http://127.0.0.1:9222/json/list` (`--remote-debugging-port=9222 --remote-allow-origins=*`), then a RAW stdlib websocket for `Page.enable`, `Runtime.evaluate`, `Page.captureScreenshot`; decode in-guest with `zlib` + the five filter types. It is chromium's OWN compositor output -- **and it TIMES OUT when the compositor presents nothing, so use `fromSurface:false`.** Never `--disable-dev-shm-usage`.
- **The client connect is what makes selkies fork** (`DPI changed from 96 to 120` -> `display_utils.py:1858` -> `create_subprocess_exec("xfconf-query",...)`; selkies runs under `/lsiopy`, NOT system python3). In chrD90 it COMPLETES with `RC: 1` (benign).
- **XWD grab: decode with the MASKS, never by byte position** (`f[14]`/`f[15]`/`f[16]` = r/g/b; pixels start at `max(len(raw) - w*h*bpp, hsize + ncolors*12)`; a GTK app's 10x10 leader window cannot be grabbed). **Every `blue=0.00%` from chrD19 and earlier is INVALID.**
- **The selkies client needs its `Play Stream` button pressed or the `<video>` never gets a track** (`readyState=0`). The a11y snapshot LIES; the real element is the first `<button>` matching `/play stream/i`, and `evaluate_script` `b.click()` works where `click` on the uid times out. Census the `<video>` in-page with a canvas, never by screenshotting into the transcript.
- **A file a fork child wrote is visible only to the shell that reaps it**: a headless `--screenshot` must run chromium in the FOREGROUND of its subshell with the analysing python after it in that SAME subshell (`chrdesk8.sh`). Blue PNG = compositor fine; white PNG = no frame.
- Never `--enable-logging=stderr --v=1` in a long run (chrD13 stopped painting at 32-37fps). Windowed recipe: `xfconf-query -c xfwm4 -p /general/use_compositing -s false`, `xsetroot -solid "#305070"`; xfce4-session takes ~120s to register its WM.
- SELKIES 2.0.0 BINDS 8080, NOT 8081; `CUSTOM_WS_PORT` is a 1.x name it ignores -- `chrdesk54.sh` passes `--port=8081 --mode=websockets`. It ENABLES BASIC AUTH BY DEFAULT and with no password STOPS ITS OWN SERVER -- always `--enable-basic-auth=false`. **A host browser reaches a guest SERVER only through `-p/--publish h:g`** (OUTBOUND NAT only); the guest sees that client as `10.0.0.1`. A host probe must not outlive the guest run or its refusals are an artifact (pub4).
- **selkies' capture REQUIRES MIT-SHM at the X server** (hard-fails on `shm_query_version`). The client's `FATAL: ... video pipeline did not start` is diagnosed by the line ABOVE it, `Failed to start capture for 'primary': <e>`. **A log selkies writes under `/tmp/` is INVISIBLE to the launching shell -- pipe it.** A/B on ONE port is useless (`kill -9` on a wedged cross-process child does not reap it -> `Address already in use`).

## Closed -- do not re-attempt without a genuinely new approach

**"`--screenshot` completes only with `--in-process-gpu`" / "the forked GPU process dies"** -- `f345821`, cb66 arm Z. **"a fork child's `dlopen` returns the wrong library"** -- `f345821`, cb64. **"a `PROT_READ` private file mapping cannot be `mprotect`ed writable"** -- cb68 F4. **"a `MAP_PRIVATE` write is visible to another process"** -- cb70. **"the compositor presents nothing"** -- `9997313`. **"a fork child does not share `MAP_SHARED`"** -- `a629714`. **"selkies dies when a browser client connects"** -- `47cefb2`, chrD90. **"the goal run needs the fd panic fixed first"** -- `4964934`, chrD92. **"a carried TCP connection loses a message"** -- the rx-delivery fix. **"the GPU process is blocked by chromium's sandbox"** -- cb21 L1: it loads zero libraries. **"a library's bytes are wrong"** -- cb14/cb15/cb37/cb38/cb39. **"a fork child cannot dlopen/dlsym"** -- cb30/cb31/cb33. **"a fork child's anonymous/heap memory is zeroed"** -- cb32. **"a fork child's inherited pointers are stale"** -- cb36/cb37. **"dconf/NSS `nspr_use_zone_allocator` misses matter"** -- cb31: those symbols are ABSENT from `.dynsym`. **"the apps census is stale / must be re-run on `4964934`"** -- apps1d/apps3d, Open #2 CLOSED. **"the published port stops answering because of the publish path or the gateway"** -- pub1/pub5: the host listener and the gateway's inbound relay are clean; it is the guest's listening backlog (this pass's fix, validation pending). Also: net_lock freeze (`16f3e76`); "spawning `xset` wedges the loop" (`463dc29`); "selkies stalls at startup" (basic auth with no password); fork admission cap 6->3 REVERTED; fd-number aliasing; "a parent-made dir is invisible to a fork child" (`homemk.sh` 30/30); chrD53-56 grey (ENVIRONMENTAL).

## Other subsystems (Linux runner / OCI, shared memory and locks, file visibility)

Moved verbatim to `docs/AGENTS_ARCHIVE_2026-10-05b.md` section 10 to keep this file under 30 kb. The one-line rules that still cost sessions: a file one host process wrote is invisible to siblings until it exits; a fork child gets the parent's writable layer AT SPAWN and its writes reach the parent only on `wait4`; extend `SPILLED_PREFIXES` rather than adding `/tmp/`.

## Docs and tooling map

Archives newest first under `docs/`: **`..._2026-10-05f.md` (this pass -- the dead listening port, pub1-pub5, the apps re-run)**, `-05e.md` (verbatim as of `37c2374` + EBADF), `-05d.md` (copy-on-write, `cowprobe`), `-05c.md` (GPU hunt cb40-cb66), `-05a.md`, `-04j/-04i/-04h/-04g/-04f/-04c.md`, the `-10-0*`/`_2026-09-*` set, `fork-region-grouping-design.md`, `track-b-fork-fix-progress.md`, `veh-exception-handler-design.md`, `advisor/ADVISORY-00{1,2}*.md`. `-05b.md` §10 holds the Linux-runner/OCI/shared-memory/file-visibility subsystem notes.
`.wfgy/` (git-IGNORED, so `codesearch` never sees it): host tooling `logscan.py`, `lockstall.py`, `scan_waiters.py`, `symstk.py`, `guest2.ps1`, `pass118_full.ps1`, `framecensus.py`, `memsamp.ps1`, `wsmap.ps1`, `th1.ps1`; publish probes `pub1*.sh`, `pub4.sh`/`pub4host.ps1`, `pub5.sh`/`pub5host.ps1`. Probes worth knowing: **`reg1.sh`** (whole fork suite in one run), `shmexec1.sh`, `homemk.sh`, **`cb65.sh`** (layered-inode acceptance), **`cb66.sh`** (THE goal test: default config, zygote ON), **`cb68.sh`** (mprotect matrix), **`cb70.sh`** (`MAP_PRIVATE` privacy), **`chrdesk54.sh`** (THE goal recipe), `apps1/apps3.sh` (+ `apps1d/apps3d.sh` on `4964934`), `chrshot4/8.sh`; the cb14-cb64 and cb40-cb49 sets are listed in archive `-05d` §1.
- gm `codesearch` (INVARIANT 4: never grep/find): `mode: "literal"` is exhaustive; `mode: "dual"` is a ranked SAMPLE, so never conclude "absent" from it. Scope with `path`/`glob` -- an unscoped literal scan caps at 40 matches and is therefore NOT exhaustive in practice. Scoping also makes it ~100x faster (16 s unscoped -> 4 ms on `./litebox/src/net`).

## Net gateway facts swept from platform/net.rs (2026-10-05f)
- Win TCP: close w/ unread rx = RST, use shutdown(Write).
- Live bugs: 843fc14 UDP flow leak; 4f1df39 inbound reply-port listener.

---

## 2. This pass (2026-10-05g): the dead published port was the listening backlog, not the publish path

### The symptom (unchanged from `-05f`, re-measured twice more)

`chrdesk55.sh` + `-Publish 8081:8081`: selkies answers `code=200` at t=30/60/90, then an in-guest
`curl 127.0.0.1:8081` is REFUSED (RST in 6-212 ms) from t=120 to the end, while

- selkies keeps encoding (`EncFPS 13.6`),
- the host browser keeps receiving the frame,
- the UNPUBLISHED 8082 `http.server` in the same guest answers `code=200` at the same instant.

chrD95 re-confirmed it; chrD96 was killed by my own next launch (`guest2.ps1` calls `Stop-Litebox`,
which kills every process under `target\release`) -- NOT by memory. chrD94/95 also showed 8082 going
empty late in the run (t=180+), i.e. the port dies there too, just later.

### What was refuted on the way (do not re-attempt)

- **The shared socket buffer pool is exhausted.** chrD95's `.err` held 144,423 lines of
  `listen backlog cannot be refilled: the socket buffer pool is exhausted port=9222` -- an
  unthrottled warn on a per-tick path (it flooded the transcript; the `diag-pool` replacement
  throttles to 1 in 512 and prints `(data_granted, data_used, meta_granted, meta_used, owners)` plus
  a per-socket `local:state:remote` census, because `Pool::new` HALVES its request until the arena
  grant succeeds, so "the pool is exhausted" can mean it was never granted more than a handful of
  slots). fl6 settled it: 40 fork children that exit WITHOUT closing a carried listener (1 main + 8
  backlog sockets each = 360 sockets, far past `MAX_SOCKETS` = 256 and past 128 TCP pool pairs) never
  exhausted anything -- `diag-pool` = 0, `socket table is full` = 0, and a brand-new listener created
  AFTER the forks (8097) accepted normally. **A fork child's carried listener IS reaped at child
  exit; there is no orphan army.**
- **A child closing a carried listening socket kills the parent's port.** fl5 (child calls
  `os.close()` on the carried listener, then exits): every probe stayed OK. fl6's CONTROL arm 8096
  (child closes it, 40 forks): OK all 40.
- **The publish path / the gateway's inbound relay.** pub1/pub5 (29/29 host connects, 45/45 guest
  200s) already cleared it in `-05f`.

### The mechanism (fl6 + `diag-listener`, then fl7 fork-free)

`diag-listener: ... port=8095 slots=8 pending=1 other=7 readable=true` -- one line, once per episode,
from `repair_listening_backlog`. It says the port has all 8 backlog slots and NOT ONE is in LISTEN.

A backlog slot whose peer's FIN arrives before the application accepts sits in **CloseWait**:

- `accept` matched `tcp::State::Established` ONLY, so it never handed that slot out;
- every sweep keeps it, because `is_open()` is still true (`Closed`/`TimeWait` are what the sweeps
  drop), so it was never reclaimed either;
- `refill_to_backlog` only runs when a slot actually leaves, so no new LISTEN socket replaced it;
- smoltcp dispatches an inbound SYN ONLY to a slot in `Listen`, so once all `backlog` (8, clamped in
  `listen()`) slots are spent the port RSTs every later SYN **forever** -- while the connections it
  already accepted keep streaming, which is exactly how Open #1 looked.

There is a second, worse half: `drain_socket_channel_buffers`'s listener-readable test also gated on
`Established`, so an epoll-driven server (selkies, asyncio) is never even TOLD about a CloseWait
connection -- it never calls `accept` at all.

fl6 (fork arm, the shape the real stack has): `8095` answered through fork 7 and was refused from
fork 8 on, `LAST_FORK_WHERE_8095_ANSWERED=7`, control `8096=OK` all 40, `FL6_RC=0`.
fl6's `PRE` line is the tell: `drained95=0 drained96=1` -- the 8095 connection was already in
CloseWait by the time `accept` ran, so it came back EAGAIN and stayed.

### The fix (`39616f4`)

1. `accept` hands out `Established | CloseWait` -- what Linux does; the application learns the peer
   is gone by reading EOF.
2. `drain_socket_channel_buffers`'s listener-readable test widened the same way, or an epoll-driven
   server never wakes.
3. `repair_listening_backlog`'s census counts `CloseWait` as pending work, not limbo.
4. `reclaim_finished_backlog_slots` (new, assoc fn next to `remove_socket`) is the safety net: a
   backlog slot left in a terminal state (`TimeWait`, `FinWait1/2`, `Closing`, `LastAck`, and
   `Closed` once its pending RST has gone out, i.e. `remote_endpoint().is_none()` -- exactly the
   condition `remove_dead_sockets` waits for) is dropped from `socket_set_handles` with
   `core::mem::forget(Self::remove_socket(...))` and refilled. A backlog slot has NO application fd
   pointing at it -- it gets one only when `accept` hands it out -- so nothing else can move it on.

### Verification

- **fl7** (`.wfgy/forklisten7.sh`, fork-free: the peer connects, writes, and closes; the server
  accepts 300 ms later): `ACCEPTED=59 PAYLOAD_SEEN=30 LAST_ROUND_WHERE_THE_PORT_ANSWERED=30`,
  `drained=2` and `probe=OK` on every one of 30 rounds, `FL7_RC=0`, zero `diag-listener`.
  `PAYLOAD_SEEN=30` is the direct proof: all 30 payloads came back through a socket whose peer had
  already hung up. (The first version of fl7 created TWO connections per round and accepted ONE, so
  it filled the backlog and died at round 9 on the FIXED binary -- a probe that out-runs its own
  accept loop is not evidence of a bug.)
- **fl6b** (same `forklisten6.sh`, same image, only the binary changed):
  `LAST_FORK_WHERE_8095_ANSWERED=40` (was **7**), `8096=OK` all 40, `LATE 8097=OK`, `FL6_RC=0`,
  zero `diag-listener`, zero `diag-pool`, zero `panicked at`.

### Harness notes this pass cost

- **Launch every guest run through `.wfgy/guest2.ps1`.** A hand-written launch silently runs with
  `LITEBOX_PROCESS_FORK` unset (cross-process fork OFF), which per `575f0e2` changes everything.
- `guest2.ps1`'s `Stop-Litebox` kills EVERY process under `target\release`, so launching run B while
  run A is live silently kills A (this is how chrD96 "died": `exit: -1`, 2.3 MB of `.err`, no
  diagnostics -- I launched fl6 at 09:41). A background PowerShell task reporting "failed exit code
  128" is the launcher's own `taskkill`, NOT the run: `.wfgy/<run>.out` is authoritative.
- An unthrottled `warn!` on a per-tick path floods the MONITOR, not just the log (144k lines ended a
  `Monitor` with "output rate too high"). Throttle with a `static AtomicU32` + `% N` before writing.
