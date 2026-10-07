# AGENTS_ARCHIVE_2026-10-05i -- a fork-family socket's RX is pulled by the reader

Section 1 is the verbatim `AGENTS.md` as it stood at the start of this pass (`-05h`, 33,498 bytes),
kept so nothing has to be recovered from git history. Section 2 is this pass's mechanism.

---

## 1. Verbatim AGENTS.md (2026-10-05h)

# litebox -- current state (2026-10-05h; recompact of `-05g`; verbatim `-05g` + this pass's mechanism in `docs/AGENTS_ARCHIVE_2026-10-05h.md`)

CURRENT-STATE index. Every claim carries a sha, `file:line`, or numbers. Shorthand: `process.rs`/`file.rs`/`mm.rs`/`unix.rs` = `litebox_shim_linux/src/syscalls/<x>`; `platform/lib.rs` = `litebox_platform_windows_userland/src/lib.rs`; `fork.rs` = `.../process_fork.rs`; `fd.rs` = `litebox/src/fd/mod.rs`; `lazy.rs` = `.../lazy_file_map.rs`; `mm/linux.rs` = `litebox/src/mm/linux.rs`; **`net/mod.rs` = `litebox/src/net/mod.rs`** (NOT `syscalls/net.rs`). **This file wins.**

## Where things stand

- **GOAL MET AND RE-MEASURED on `4964934` (chrD92)**: sandboxed chromium -- its OWN sandbox, no `--no-sandbox` -- renders VISIBLE IN A REAL HOST BROWSER (`chrdesk54.sh` + `-Publish 8081:8081`). Host-browser frame `1354x834 readyState=4 blue=42.41% root[48,80,112]=53.91% white=0.00%`; guest xwd `blue=99.79%`; CDP `shot 800x600 blue=99.78%`; **ZERO `panicked at`**. chrD90 (`47cefb2`): `1354x832 blue=42.22%/root=54.11%/grey=0/white=0`.
- **chrD97 (`39616f4`) split Open #1 in two, and the second half now has a measured cause: the SHARED SOCKET BUFFER POOL.** (a) The backlog fix holds: `selkies=code=200` AND `httpd8082_unpublished=code=200` at t=30 and t=60. (b) From t=90 in-guest `curl 127.0.0.1:8081` refused (RST in 8-41 ms) while a brand-new HOST connection to the same published port loaded and selkies encoded at ~14 EncFPS. `diag-pool` (18x, throttled 1/512 => ~9,200 failed refills) read `data_granted=256 data_used=256 owners=128 sockets=128` -- **104 of those 128 were merely armed backlog slots of ~13 listening ports**. Fix applied, goal re-run pending. Host frame `1360x836 rs=4 root[49,80,112]=54.77% grey246=41.24% blue=0.00%`; guest xwd agrees (`w90.xwd 800x600 grey246=100%`) -- the `--app` window paints the DEFAULT grey, not a.html's `#20c0f0`, though the httpd served it 5 times.
- **HEADLESS TOO (cb66 arm Z, `f345821`)**: DEFAULT config -- zygote ON, chromium's OWN sandbox, GPU process FORKED from the zygote -- `PNG_APPEARED i=6`, `800x600 #20c0f0=99.95%`, `CHROMIUM_RC=0`. `--in-process-gpu` is only the control.
- **APPS RE-VERIFIED on `4964934`** (apps1d/apps3d, zero panics): `xterm` red 97.4%, `xclock` blue 95.1%, `xcalc` white 83.2%, `xeyes` 155 colours, `xedit` white 94.2%, chromium `--app` `240,192,32 99.8%`, `mousepad` white 87.4%, `thunar` white 60.9%. **Open #2 CLOSED.**
- **Branches**: `inetfix` == `39616f4` on top of `4964934`/`37c2374`/`7c1d987`; **local `main` is still at `47cefb2` -- fast-forward pending, never done automatically.**
- **THE SELKIES DEATH IS `47cefb2`, NOT HOST RAM**: `DescriptorEntry::as_subsystem_mut()` (`fd.rs:1095`) panicked the whole guest process at selkies' first post-connect fork. The chrD79-85 "ENVIRONMENTAL, host RAM" verdict is WITHDRAWN. `Could not obtain XFCE session environment` = the fork child died (survivable, chrD90 `RC: 1`); host-RAM starvation is separate. **Check free RAM first regardless.**
- Judge a host-browser frame only after ~60s of STREAM time (chrD32 `white=42.61%` -> `blue=42.23%` at t=61s); white/grey is PRE-FIRST-PAINT. Guest time dilates 3-4.6x vs wall; attach a host browser only to take the frame.
- **Before ANY run**: sweep runners by CIM (`Get-CimInstance Win32_Process -Filter "Name like 'litebox%'"`), close other host browsers, require >=2.9GB free. `taskkill /F /T` did NOT kill a 1013MB runner; `Stop-Process -Force -Id` did (some are undeletable but killable). **Leave gm's `agentplug-runner.exe daemon` alone.** Sweeping 52 leaked runners took free RAM 2596 -> 3573 MB.

## Fixed, newest first (mechanism per sha in `docs/AGENTS_ARCHIVE_*`; keep only the RULE)

- **THE SHARED SOCKET BUFFER POOL, NOT THE SOCKET TABLE, IS WHAT A DESKTOP SESSION RUNS OUT OF (this pass).** chrD97: 128 sockets held all 256 granted data slots while `MAX_SOCKETS` is 256, and 104 of the 128 were armed backlog slots. A failed `refill_to_backlog` is permanent, so every listening port slowly goes deaf while its accepted connections stream on -- exactly Open #1's symptom. Fix, at the SAME 16 MiB: `MAX_DATA_SLOTS` 256 -> `2 * MAX_SOCKETS` (512) and a new `SOCKET_RING_SIZE` 32768 (was `SOCKET_BUFFER_SIZE` 65536) -- capacity 128 -> 256 TCP sockets, measured by `.wfgy/sockcap.sh` (`SOCKCAP_TCP=256 stopped=EMFILE`; the pre-change binary stops at 128). RULE: **one listening port costs one smoltcp socket PER backlog slot (`listen()` clamps `backlog.max(1).min(8)`), so socket budget is mostly backlog.**
- **`TooManySockets` IS `EMFILE`, NOT A PANIC (this pass).** `litebox_common_linux/src/errno/mod.rs` mapped only `UnsupportedProtocol`, so it fell through to the shim's `unimplemented!()` -- three guest processes died on it in chrD97, each taking its listening ports with it. Same pass: `connect`'s `check_state` no longer `unimplemented!()`s on a TCP state (a completed handshake the peer is already closing is `Ok`; `Closed`/`TimeWait` are `InvalidState`).
- **`39616f4` A PEER-CLOSED CONNECTION MUST COME OUT OF THE ACCEPT QUEUE (the dead published port).** A backlog slot whose peer's FIN arrives before the application accepts sits in **CloseWait**: `accept` matched `Established` only and every sweep keeps it (`is_open()` is true) -- never handed out, never reclaimed, one slot lost per connection, and after `backlog` of them the port has no slot in LISTEN and **RSTs every later SYN forever while its accepted connections keep streaming**. `accept` now hands out `Established | CloseWait` (Linux does; the app learns the peer is gone by reading EOF). Two gates widened with it: **`drain_socket_channel_buffers`'s listener-readable test** (an epoll-driven server otherwise never accepts AT ALL -- the half that killed selkies) and `repair_listening_backlog`'s census. Safety net: `reclaim_finished_backlog_slots` drops a slot left in a terminal state (`TimeWait`, `FinWait*`, `Closing`, `LastAck`, and `Closed` once its pending RST is out) and refills. fl7: `ACCEPTED=59 PAYLOAD_SEEN=30 LAST_ROUND_WHERE_THE_PORT_ANSWERED=30`; fl6 A/B: `LAST_FORK_WHERE_8095_ANSWERED` **7 -> 40**. **THIS CHANGES WHAT `accept(2)` RETURNS**: any probe that connect-then-closes leaves a peer-closed connection in the queue and `accept` hands it out first -- drain the queue before measuring (`.wfgy/inetfork2.sh`'s `drain()`), or you measure the wrong connection. Locked in by the new reg1 subtest `peer_closed_conn_comes_out_of_queue`.
- **`2df5677` A LISTENING PORT RE-ARMS ITSELF FROM THE TICK, NOT ONLY FROM `accept`.** With every slot gone the port has no listener, no SYN can make a slot `Established`, so the readable re-arm never fires, so the server never calls `accept`, so nothing re-arms. Necessary but NOT sufficient -- `39616f4` is what stops the slots being spent.
- **`4964934` A CLOSE OF A NUMBER NOTHING OWNS IS EBADF, NEVER A PANIC.** chrD91 died on `unreachable` at `fd.rs:141` (`epoll` stores `TypedFd`s and holds no file reference). Same treatment for `Descriptors::remove` (`fd.rs:114`), `Network::close`'s Deferred arm, `do_close_and_replace`'s fallthrough (`file.rs:4679`). **reg1e byte-identical to reg1d.**
- **A CARRIED SHARED REGION MUST REACH THE CHILD WITH ITS OWN PROTECTION, NOT THE WIDEST (`37c2374`).** `VirtualProtectEx` down to `prot_flags(carry.perms)` while the child is suspended; failure is reported, not fatal.
- **A PRIVATE FILE MAPPING IS A COPY-ON-WRITE VIEW OF ONE SHARED SECTION (`7c1d987`).** `VmFlags::VM_PRIVATE_FILE_COW` (bit 11) -> `COPY_ON_WRITE` -> `PAGE_WRITECOPY`/`PAGE_EXECUTE_WRITECOPY`, `MAP_PRIVATE` not `MAP_SHARED`. So `mprotect(PROT_READ|PROT_WRITE)` on a `PROT_READ` mapping SUCCEEDS, the write is this process's own, and the 324 MB binary stays ONE copy. **cb68 F4/F4B/F4C/F4D/P2 `rc=0 errno=0`; cb70 privacy holds both directions, `F_FILE_UNMODIFIED=True`.** RULE: **the qualifier travels with the VMA** (`insert_mapping`, `protect_mapping`, `duplicate`, `shared_region_carry`) and the fork PARENT's `MapViewOfFile3` must ask `PAGE_*_WRITECOPY`.
- **A `PROT_NONE` FILE MAPPING IS A RESERVATION, NOT A SHARED READ-ONLY VIEW** (cb45 F2 EACCES in parent AND child). Fix: exclude `prot.is_empty()` (cb45b `rc=0 errno=0`). `a2ac697`: it must still carry the file's bytes.
- **`f345821` AN INODE NUMBER MUST NAME THE FILE, NOT COUNT THE PATHS THIS PROCESS HAS STAT'ED.** A fork child rebuilt its fs with an EMPTY lookup table, and **ld.so decides "already loaded" by `(st_dev, st_ino)` vs the PARENT's numbers** -- cb64 watched a byte-identical COPY of libEGL.so.1 come back AS libm, every `dlsym` NULL, GPU process dead on `call *rax` with `rax=0`. Fix: `layered_ino()` = FNV-1a over `(dev, ino, rdev)`, forced odd (cb65 `IDENT_CHANGED_VS_PARENT=0 of 69`). RULE: **any number a guest can observe across processes must be a function of the file, never of this process's history.**
- **A CARRIED SOCKET'S RX MUST REACH THE PROCESS THAT IS READING IT.** BOTH halves were needed: (1) the shared tick skips the TCP **and** UDP rx drain when `shared && !proxy.has_observers()`; (2) `receive` calls `drain_rx_into_proxy(fd)` first when shared -- a NON-BLOCKING read registers no observer, so (1) alone starves it. Residual: both referents observing still race.
- **`47cefb2` A MISTYPED FD IS EBADF, NEVER A PANIC** (every `TypedFd` resolver gates on `matches_subsystem::<Subsystem>()`); `4964934` extends it to the close path. **`reset_pages` no longer `unimplemented!()`s on `madvise(MADV_DONTNEED)` over a file-backed range** (a panic there kills the session).
- Older, still binding: `9997313` INET crosses a cross-process fork, and **FD_CLOEXEC is about `execve()`, not `fork()`** (rejections log at `warn!`); `a629714` a fork child SHARES `MAP_SHARED` -- **validate every segment BEFORE reserving the group placeholder**; `6183f53` an unplaceable page-granular hole RELOCATES a `Hint`; `f260226` cross-process `flock(2)`, keyed `(dev, path)` NOT `(dev, ino)`; `6a472b5` `shmat` on an `IPC_RMID`'d-but-still-attached segment SUCCEEDS; `f5d73ff` the flock registry is per host process -- **that panic correlated with LAUNCH ORDINAL, invalidating every A/B in that window**; `575f0e2` an uncarriable fd silently drops the fork off the cross-process path, so **never run the stack with fork OFF**; `aca3a53`/`9f7c994` one store per object / sized memfd; `ae6926d` (branch `drmevdev`, unmerged) shared DRM/evdev tables.
- Pipes/locks/waiters: `31ca26b` `MAX_INLINE_WAITERS` 32->128; `1732ad9` a carried-pipe pump must not outlive its child; `463dc29` **never drain a pipe the guest holds for itself** (libuv's GLOBAL SIGNAL LOCK); `16f3e76` **no path holding `net_lock` may block**; `a1423ee` HOLDING is not READING; `7d2a6a7` `SHARED_UNIX_CONN_CAPACITY` 1024->4096; `efce1a5` `/proc` enumerates the whole session (`guest_pids=30`; before **1**).

## Diagnosis: the listening-port path

`diag-listener` (once per episode, `repair_listening_backlog`): `port=8095 slots=8 pending=1 other=7 readable=true` -- every slot spent, none in LISTEN. `diag-pool` (throttled 1/512, `refill_to_backlog`): `(data_granted, data_used, meta_granted, meta_used, owners)` + a per-socket `local:state:remote` census -- `Pool::new` HALVES its request until the arena grants, so "exhausted" can mean it was never granted more than a handful of slots. `diag-accept` (a stale slot dropped in `accept`). **`forklisten6.sh`** = the fork arm (child exits WITHOUT closing a carried listener; 8095 vs 8096 control vs 8097 created after the forks), **`forklisten7.sh`** = the same mechanism fork-free (peer hangs up 300 ms before the accept). **`.wfgy/sockcap.sh`** = how many TCP sockets the pool actually holds. **`.wfgy/errshapes.py <file>`** = dedupes a `.err` into message shapes with counts and first line numbers (normalises digits/hex); use it instead of remembering a census.

## Open, in rough priority order

1. **Re-run the goal stack with the pool fix** (`chrdesk55.sh` + `-Publish 8081:8081`): the in-guest `curl 127.0.0.1:8081` must answer through t=420 and `selkies=code=200` must hold. The refusal's cause is measured (pool exhaustion); the fix is applied; the goal re-run is what closes it.
2. **chrD83: 2 x `Exception(14) error_code=0x15`** = user-mode instruction fetch to a PRESENT non-executable page, distinct from the `0x7` relocating-fork signature. chrD79/85 have ZERO and chrD83 ran starved beside 16 `chrome.exe`; **re-measure on a clean host before calling it a litebox bug.** `LITEBOX_DIAG_FAULT_VQ=1` shows `PAGE_READONLY` where `PAGE_EXECUTE_READ` is expected; `prot_flags` (`platform/lib.rs`) is CORRECT. `cb9.sh`/`cb19.sh` never run.
3. A real `flock(2)` consumer (chromium's profile lock, SQLite) -- `f260226` is verified by `.wfgy/flockx1.sh` only.
4. Host memory is the binding constraint, not a litebox bug. **Check host RAM FIRST.** `LITEBOX_DIAG_ALLOC_STACK=1`, `_MEM_BREAKDOWN=1`; `memsamp.ps1`, `wsmap.ps1 -ProcId`. **Snapshot-binary runners leak: sweep by CIM, not by name.**
5. **The 128MiB shared kernel arena is nearly full from ~6 concurrent `GlobalState`s** (each 15-21MB, dominated by `SharedUnixConnTable`): `arena exhausted` hits ~1 run in 5 at BASELINE -- size any new arena-resident table against that.
6. Verify in the full stack (committed, never seen in a browser): `3e1ef47` SCM_RIGHTS over a cross-process unix connection; `b012910` lazy file map stale entries; `08ae94f` `/proc/<pid>/fd`. Repro `.wfgy/th1.ps1 -Run th1|th2|th3`.
7. Native kernel-COW fork on Windows (`.gm/prd.yml` `native-kernel-cow-fork`); writable layer shared across processes; AF_UNIX exhaustion still silent (256 slots, keys >108 bytes); `timerfd`/`signalfd` uncarriable. Chromium `Network service crashed ... restarting` (chrD97) is unexamined. `pub6host.ps1` `HOST_OK=0 HOST_FAIL=4` (4 attempts in 300 s) is unexplained -- possibly a stale host listener.
8. Chromium startup flakiness: chrD56 rc=0 with `crashpad ... mkdir .../Crash Reports: No such file or directory (2)`, never mapped a window -- `homemk.sh`: a parent-made dir chain IS visible to a `setpriv --reuid 911` child 30/30, so timing, not visibility.
9. **Fast-forward local `main` to `inetfix`** (`main` is at `47cefb2`). Only ever push to `origin` with per-instance confirmation.

## How to run and drive it (harness lessons that cost sessions)

- **No WSL, no hypervisor, ever.** Native Win32 exe; host tooling PowerShell or Git Bash; launches `CREATE_NO_WINDOW`.
- **ALWAYS launch through `.wfgy/guest2.ps1`** -- a hand-written launch silently runs with fork OFF (`575f0e2`). It sets `LITEBOX_PROCESS_FORK=1`, `_LAZY_FILE_MAP=1`, `_OCI_USE_LAST_RESOLVED=1`; `-Script <sh> -Run <name> -Secs <n> [-Publish h:g] [-ExtraEnv] [-Exe]`. **Its `Stop-Litebox` kills EVERY process under `target\release`, so launching run B while run A is live silently kills A** (this is how chrD96 "died"). A background task reporting "failed exit code 128" is that `taskkill`, NOT the run -- `.wfgy/<run>.out` is authoritative.
- Cheap repro: `target/release/litebox_runner_linux_on_windows_userland.exe -Z --oci-image docker.io/library/debian:stable-slim -- /bin/bash -c '<script>'`. PowerShell never Git Bash for guest paths; single quotes only inside `-c`. **Build ONLY `cargo build --release -p litebox_runner_linux_on_windows_userland`. Never pipe a build through `tail`.**
- **The OCI layer cache is `./.litebox-cache` at the repo ROOT (24 GB)**; `target/release/.litebox-cache` holds only `boot.lock`. Keys are versioned (`_v2.tar`); after a format bump every layer logs `[cache] MISS ... (v2)` and the run re-pulls (~30-60 min) -- check `.err` for `MISS` before calling a run hung. **Watch disk.**
- Full stack: `.wfgy/pass118_full.ps1 -Run <name> -MaxSeconds N`. **`rc=137` at the END of a run is the harness cap, not a bug.**
- **Chromium does NOT exit after a failed `--screenshot`**: `wait` blocks to the cap and STARVES later arms. **KILL the arm (`kill -9 $ARM; pkill -9 -x chromium`)** and keep a capture's deadline BELOW the PNG poll window. **`timeout(1)` HANGS in-guest -- it is NOT a bound**; bound with `-Secs` or an in-guest background+wait loop, and never conclude "X hangs" from a cap-killed run without per-step BEFORE/AFTER prints.
- **`$?` after a pipeline is the LAST command's status**; **an unquoted heredoc expands `${PIPESTATUS[0]}`/`$(date +%s)` at FILE-WRITE time**; **a nested heredoc terminator hangs the script -- write probe files with the Write tool**; **Bash heredocs collapse `\\` -> `\`**, build Python escapes with `chr(92)`. **Guest scripts must be LF** -- count CR bytes in python, NOT `grep -c $'\r'`. **A probe that cannot fail is not evidence** -- include a negative control (and one that out-runs its own accept loop measures the probe, not the stack: fl7 v1 died at round 9 on the FIXED binary). `socket.timeout` has `errno=None`, so `"errno=%d"` raises TypeError and the traceback REPLACES the verdict (use `%s`) -- it did, and took the rest of inetfork1's arms with it. **Two processes reading ONE socket race over the same bytes**: give each direction its own stream (`cli`->`srv` one way, `srv`->`cli` the other).
- Cargo cannot relink while a runner holds the exe (`os error 5`): kill every runner first; if the lock persists **rename** the exe, build, delete `.prev*`. **Check the binary is NEWER than the commit under test.** In PowerShell `$?` is False after a successful `cargo build` whose stderr was redirected -- read the log's last line. Unkillable runner holding the boot lock: `Remove-Item -Force target/release/.litebox-cache/boot.lock`.
- `rc=137` from a fork child does NOT prove a kill (`decode_cross_process_exit_status`, `process.rs:390-403`). `.err` SIZE is run health; `chrD*.err` is BINARY to gm `codesearch` -- use `.wfgy/logscan.py <file> <term>`. **Never dump a raw image into the transcript** -- census in-page/in-guest.
- **An unthrottled `warn!` on a per-tick path floods the MONITOR, not just the log** (144,423 lines of one line ended a `Monitor` with "output rate too high"). Throttle with a `static AtomicU32` + `% N` before writing.
- **Guest probe constraints**: there is **NO C compiler in the image** (`gcc` -> rc 127), so every probe must be python; `dlopen("/usr/lib/chromium/chromium")` fails anyway. **`python3` in a `guest2.ps1` run is itself a cross-process fork child** (`pid=3 ppid=1`). `readelf --dyn-syms` TRUNCATES long names and versioned symbols carry `@`; use `readelf -W`, strip `@`, skip `[`. **`LITEBOX_*` are host-side runner vars -- invisible in-guest.** **`/proc/net/tcp` DOES NOT EXIST** (pub4: `FileNotFoundError(2)`).
- Diagnostics: `cdb -pv -p <pid> -xd av -xd sse -c "~*kb 25; qd"` (NOT on PATH); `litebox_diag::{process_timeline,socket_read,unix_conn_teardown,stderr_capture}=debug`. The image's Xvfb is XLibre and ABORTS when `/tmp/.X11-unix` exists with the wrong mode -- `mkdir -p /tmp/.X11-unix && chmod 1777` BEFORE Xvfb; dbus-daemon non-forking; use `xfce4-session`. **A pipe the guest holds for itself must be probed with a zero-timeout `select()`, never `read`**; asyncio closes a child stdin the instant it exits -- keep `p.stdin`.
- Repo hygiene: tars, frame dumps, debug logs never in git (`.wfgy/` ignored); **commit as lanmower only, no `Co-Authored-By`**; **never `git push` to `origin` (github.com/AnEntrypoint/litebox) without per-instance confirmation.** `4792fc5` pins `*.rs`/`*.toml`/`*.md`/`*.lock` to LF -- a wholesale CRLF flip blew `git diff` up to ~27k lines. **INVARIANT 3: at >30 kb, recompact (verbatim old text into `docs/AGENTS_ARCHIVE_<date><letter>.md`, then cut to rules).**

## Standing lessons and hard constraints

- Guest-reachable code returns an errno, never a panic (the host process IS the whole session). Refusal errno is contract: EPERM degrades, EINVAL/ENOSYS fails hard.
- `LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check. Never subtract timestamps across a parent and a fork-child log (`init_logging()` resets elapsed time per child).
- **A `TypedFd` index is valid only against the `Descriptors` that inserted it, and only for its own subsystem** (`fd.rs:1095`, `net.rs:1194`). Shared-memory structs must hold no process-relative pointers. **A per-process object (a socket proxy) cannot be the delivery target for another process's data.** A `SharedUnixAddrPresenceTable`-shaped mutable table needs every write path mirrored to the shared side. Any per-tick sweep over shared state must not DROP an object another process allocated (wrong heap; see `remove_dead_sockets`).
- **A backlog slot of a listening port is reachable by NOTHING but `accept`** -- no fd, no proxy. Anything that leaves one in a state `accept` will not hand out costs the port capacity permanently, and a port with no slot in LISTEN refuses every SYN while its accepted connections stream on.
- **`(dev, ino)` is cross-process-stable for IMAGE files ONLY (`f345821`)**, never for runtime-created ones. **Keep keying shared registries on `(dev, path)`** -- `f260226`'s flock table depends on it. cb69: `IMAGE_MATCH=YES`, runtime `RUNTIME_MATCH=0 of 4` but `CHILD_COLLISIONS=0 of 9` -- a wrong number is silent, never a crash.
- Cross-process fork (`LITEBOX_PROCESS_FORK=1`): carried = pipes/regular files/eventfds/pty/unix sockets + INET (`9997313`) + `MAP_SHARED` (`a629714`); dropped = pty fds and non-INET cloexec fds. Slots (`CROSS_PROCESS_FORK_SLOT_COUNT` 6) gate the spawn and fail open after 8s; keyed by child host pid, freed at STARTUP-COMPLETE not at reap (`a24ce9d`). The env var is PRESENCE-CHECKED (`var_os`): `=0` still ENABLES it. The post-duplication clone site is SUPERSEDED -- believe `try_cross_process_fork`'s log.
- **Lazy file map**: chunks >= `MIN_LAZY_LEN` (256 KiB) are armed `PAGE_NOACCESS` and VEH-filled from a `'static` source slice keyed by OCI LAYER INDEX (`register_layer_sources`). **A guest reads guest strings through a bare host-side dereference with NO validation** -- `Some("")` means a real 0x00 byte, `None` means the read faulted.
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
- **XWD grab: decode with the MASKS, never by byte position** (`f[14]`/`f[15]`/`f[16]` = r/g/b; pixels start at `max(len(raw) - w*h*bpp, hsize + ncolors*12)`). **Every `blue=0.00%` from chrD19 and earlier is INVALID.**
- **The selkies client needs its `Play Stream` button pressed or the `<video>` never gets a track** (`readyState=0`). The a11y snapshot LIES; the real element is the first `<button>` matching `/play stream/i`, and `evaluate_script` `b.click()` works where `click` on the uid times out. Census the `<video>` in-page with a canvas, never by screenshotting into the transcript.
- **A file a fork child wrote is visible only to the shell that reaps it**: a headless `--screenshot` must run chromium in the FOREGROUND of its subshell with the analysing python after it in that SAME subshell (`chrdesk8.sh`). Blue PNG = compositor fine; white PNG = no frame.
- Never `--enable-logging=stderr --v=1` in a long run (chrD13 stopped painting at 32-37fps). Windowed recipe: `xfconf-query -c xfwm4 -p /general/use_compositing -s false`, `xsetroot -solid "#305070"`; xfce4-session takes ~120s to register its WM.
- SELKIES 2.0.0 BINDS 8080, NOT 8081; `CUSTOM_WS_PORT` is a 1.x name it ignores -- `chrdesk54/55.sh` pass `--port=8081 --mode=websockets`. It ENABLES BASIC AUTH BY DEFAULT and with no password STOPS ITS OWN SERVER -- always `--enable-basic-auth=false`. **A host browser reaches a guest SERVER only through `-p/--publish h:g`** (OUTBOUND NAT only); the guest sees that client as `10.0.0.1`. A host probe must not outlive the guest run or its refusals are an artifact (pub4). pub6: a guest `ThreadingTCPServer` on a PUBLISHED port answered `code=200` on all 60 ticks over 300 s under a host hammer -- the published port itself does not die.
- **selkies' capture REQUIRES MIT-SHM at the X server** (hard-fails on `shm_query_version`). The client's `FATAL: ... video pipeline did not start` is diagnosed by the line ABOVE it, `Failed to start capture for 'primary': <e>`. **A log selkies writes under `/tmp/` is INVISIBLE to the launching shell -- pipe it.** A/B on ONE port is useless (`kill -9` on a wedged cross-process child does not reap it -> `Address already in use`).

## Closed -- do not re-attempt without a genuinely new approach

**"`--screenshot` completes only with `--in-process-gpu`" / "the forked GPU process dies"** -- `f345821`, cb66 arm Z. **"a fork child's `dlopen` returns the wrong library"** -- `f345821`, cb64. **"a `PROT_READ` private file mapping cannot be `mprotect`ed writable"** -- cb68 F4. **"a `MAP_PRIVATE` write is visible to another process"** -- cb70. **"the compositor presents nothing"** -- `9997313`. **"a fork child does not share `MAP_SHARED`"** -- `a629714`. **"selkies dies when a browser client connects"** -- `47cefb2`, chrD90. **"the goal run needs the fd panic fixed first"** -- `4964934`, chrD92. **"a carried TCP connection loses a message"** -- the rx-delivery fix. **"the GPU process is blocked by chromium's sandbox"** -- cb21 L1. **"a library's bytes are wrong"** -- cb14/cb15/cb37/cb38/cb39. **"a fork child cannot dlopen/dlsym"** -- cb30/cb31/cb33. **"a fork child's anonymous/heap memory is zeroed"** -- cb32. **"a fork child's inherited pointers are stale"** -- cb36/cb37. **"dconf/NSS `nspr_use_zone_allocator` misses matter"** -- cb31 (those symbols are ABSENT). **"the apps census is stale"** -- apps1d/apps3d. **"the published port stops answering because of the publish path or the gateway"** -- pub1/pub5 (29/29 host connects, 45/45 guest 200s). **"a fork child that exits without closing a carried listener starves the shared socket buffer pool"** -- fl6: 40 such children, `diag-pool`=0, a listener created after the forks accepts; the child's sockets ARE reaped at exit. **"a child closing a carried listening socket kills the parent's port"** -- fl5, and fl6's CONTROL arm 8096 OK all 40. **"the published port dies because the backlog cannot be re-armed"** -- `2df5677` alone did not fix it; the slots were being spent (`39616f4`, fl6 7 -> 40, fl7 30/30). **"`39616f4` regressed `reg1.sh`"** -- bisected: the two inetfork2 failures were the PROBE measuring a stale peer-closed connection, not the code; fixed by draining the accept queue (and inetfork1's `tcp_parent_to_child` was two processes reading ONE socket). Also: net_lock freeze (`16f3e76`); "spawning `xset` wedges the loop" (`463dc29`); "selkies stalls at startup" (basic auth with no password); fork admission cap 6->3 REVERTED; fd-number aliasing; "a parent-made dir is invisible to a fork child" (`homemk.sh` 30/30); chrD53-56 grey (ENVIRONMENTAL).

## Other subsystems (Linux runner / OCI, shared memory and locks, file visibility)

Moved verbatim to `docs/AGENTS_ARCHIVE_2026-10-05b.md` section 10. The one-line rules that still cost sessions: a file one host process wrote is invisible to siblings until it exits; a fork child gets the parent's writable layer AT SPAWN and its writes reach the parent only on `wait4`; extend `SPILLED_PREFIXES` rather than adding `/tmp/`.

## Docs and tooling map

Archives newest first under `docs/`: **`..._2026-10-05h.md` (this pass -- the pool exhaustion, `sockcap`, the accept-queue probe semantics, verbatim `-05g`)**, `-05g.md` (accept-queue root cause, fl1-fl7, refuted hypotheses, verbatim `-05f`), `-05f.md` (the dead listening port, pub1-pub5, apps re-run), `-05e.md` (EBADF), `-05d.md` (copy-on-write, `cowprobe`), `-05c.md` (GPU hunt cb40-cb66), `-05a.md`, `-04j/-04i/-04h/-04g/-04f/-04c.md`, the `-10-0*`/`_2026-09-*` set, `fork-region-grouping-design.md`, `track-b-fork-fix-progress.md`, `veh-exception-handler-design.md`, `advisor/ADVISORY-00{1,2}*.md`. `-05b.md` §10 holds the Linux-runner/OCI/shared-memory/file-visibility notes.
`.wfgy/` (git-IGNORED, so `codesearch` never sees it): host tooling `logscan.py`, **`errshapes.py`**, `lockstall.py`, `scan_waiters.py`, `symstk.py`, `guest2.ps1`, `pass118_full.ps1`, `framecensus.py`, `memsamp.ps1`, `wsmap.ps1`, `th1.ps1`; publish probes `pub1*.sh`, `pub4.sh`/`pub4host.ps1`, `pub5.sh`/`pub5host.ps1`, `pub6*.sh`/`pub6host.ps1`; listener probes `forklisten1-7.sh`; **`sockcap.sh`** (pool capacity), **`inetfork2.sh`** (standalone fork+INET). Probes worth knowing: **`reg1.sh`** (whole fork suite in one run), `shmexec1.sh`, `homemk.sh`, **`cb65.sh`**, **`cb66.sh`** (THE goal test: default config, zygote ON), **`cb68.sh`**, **`cb70.sh`**, **`chrdesk54.sh`** (THE goal recipe; `chrdesk55.sh` = + the 8081/8082 HOLD probe), `apps1/apps3.sh` (+ `apps1d/apps3d.sh`), `chrshot4/8.sh`; the cb14-cb64 and cb40-cb49 sets are listed in archive `-05d` §1.
- gm `codesearch` (INVARIANT 4: never grep/find): `mode: "literal"` is exhaustive; `mode: "dual"` is a ranked SAMPLE, so never conclude "absent" from it. Scope with `path`/`glob` -- an unscoped literal scan caps at 40 matches and is therefore NOT exhaustive in practice. Scoping also makes it ~100x faster (16 s unscoped -> 4 ms on `./litebox/src/net`).
- Net gateway facts swept from `platform/net.rs` (`-05f`): Win TCP close with unread rx = RST, use `shutdown(Write)`; live bugs `843fc14` UDP flow leak, `4f1df39` inbound reply-port listener.

---

## 2. This pass (`-05i`): a fork-family socket's RX is pulled by the reader, never pushed by the tick

### 2.1 The symptom

`reg1g` (full fork regression on the pool change `MAX_DATA_SLOTS = 2 * MAX_SOCKETS`) broke two arms
that had passed for the whole session:

```
[inet.tcp_parent_to_child] FAIL recv=b''            (expected b'pong-from-parent')
[inet.parent_survives_child_exit] FAIL recv=b'pong-from-parent'
```

The second one is the tell: the parent had just written `pong-from-parent` on `cli` and the child
was supposed to read it on `srv`. Instead the PARENT read its own pong back off `srv` 20 seconds
later. So the bytes existed, arrived in the guest, and were delivered into the wrong process's
buffer -- not lost, not dropped by the pool, not misrouted by smoltcp.

### 2.2 The measurement that named it (`.wfgy/ifrx1.sh`, run `ifrx1a`)

Four combinations: which end the child reads (`srv` or `cli`) x whether the parent had ever
performed a blocking read on `srv` before forking (`pre_read=0/1`).

```
[parent srv pre_read=1] wrote PONG on cli
[child  srv pre_read=1] NO TimeoutError('timed out')     <-- child's recv returned EOF, not the bytes
[parent srv pre_read=1] leftover srv=b'PONG'              <-- the bytes were in the PARENT's proxy
```

So: after the parent has ever blocked on a socket, the tick hands that socket's RX to the PARENT's
proxy even while the CHILD is the one blocked reading. `pre_read=0` was clean, which rules out
wrong-end delivery and rules out pool corruption -- the difference is purely the parent's wait
history.

### 2.3 Why the old guard could not work

The guard being replaced was `if !(shared_across_fork && !proxy.has_observers())`. Two things
wrong with it:

1. **A proxy is a per-process object.** `StreamSocketChannel` lives on the process heap that created
   it. A socket carried across a cross-process fork has TWO proxies (one per process heap) over one
   smoltcp socket. Bytes the tick pushes into the parent's proxy are unreachable from the child --
   its reader waits forever while the bytes sit in a buffer no poll ever looks at.
2. **`Subject::has_observers()` is `nums != 0` and an observer is never unregistered.**
   `wait_on_events` (`event/polling.rs`) registers and never unregisters; dead `Weak` observers are
   pruned only inside `register_observer`/`notify_observers`. So a process that has ever blocked on
   a carried socket -- or merely `connect()`ed it -- counts as "waiting here" forever, and goes on
   eating its child's bytes. `event/observer.rs` says as much: "Deliberately the loose answer --
   can over-report." Any heuristic of the form "is anybody waiting?" is therefore unsound; the only
   sound question is "who is asking right now".

### 2.4 The fix: pull-only RX for fork-shared sockets

The tick no longer decides. For a socket marked `shared_across_fork` it leaves the bytes in smoltcp
and only publishes a flag:

* `Network::drain_socket_channel_buffers` gains `pull_rx: bool`. TCP and UDP RX move into the proxy
  only when `!shared_across_fork || pull_rx`. Every tick, close-flush and `shutdown(SHUT_WR)`-flush
  call site passes `false`. Only `Network::drain_rx_into_proxy` passes `true`, and it is reached
  from exactly one place: `receive` in `syscalls/net.rs`, on the socket a process is actually
  reading, in that process.
* `StreamSocketChannel`/`DatagramSocketChannel` gain `smoltcp_rx_pending: AtomicBool`, set from
  `tcp_socket.can_recv()` after each drain. `is_readable()` ORs it in, so `poll`/`epoll`/`select`
  still report readable and a waiter runs the read that fetches the bytes;
  `set_smoltcp_rx_pending` notifies `Events::IN` on the false->true edge.
* `check_io_events` (datagram) now uses `is_readable()`, so both families report the same way.
* `has_observers()` was removed from both channels -- it is now unused, and leaving it would invite
  the same unsound guard back.

Waiting still works: `wait_on_events_polling` (`syscalls/unix.rs:3628`) re-polls every
`SHARED_UNIX_POLL_INTERVAL` and calls `try_op` each iteration, so a blocking reader on a shared
socket pulls for itself instead of waiting for a tick that will never push to it.

Two hazards handled while editing:

* `drain_rx_into_proxy` must pass the socket's REAL `shared_across_fork` marking (computed with
  `self.is_shared_across_fork(entry.entry.handle)`), or `smoltcp_rx_pending` would stay stuck true
  and the socket would report readable forever with nothing to read.
* `is_shared_across_fork` is set by `set_socket_proxy` at adopt time, so a child's very first read
  already pulls.
* The flag is only maintained when `shared_across_fork` is true, so the non-shared "proxy ring
  full, bytes stay in smoltcp" case cannot set it and cannot busy-spin a waiter.

### 2.5 Verification

* `ifrx1b`: all four combinations clean -- child `GOT b'PONG' after 1.8-2.0s`, and every
  `leftover` line in both processes is `TimeoutError('timed out')`, i.e. nothing stranded anywhere.
* `reg1h`: **46 PASS / 0 FAIL**, both previously-failing arms now pass, and the post-reap
  `MSG_PEEK` diagnostics read `peek cli=TimeoutError('timed out')` / `peek srv=TimeoutError(...)` --
  nothing left behind in either end of the connection.
* The pool change from the previous pass was ruled out: it cannot alias slots, and the failure
  depended on wait history, not on slot allocation.

### 2.6 Also fixed on the way

* `SocketError::TooManySockets => Errno::EMFILE` (was `unimplemented!()` -- a panic reachable from
  a guest that opens too many sockets).
* `inetfork2.sh` `roundtrip` no longer races: the child closes the inherited listener before
  connecting, because a carried LISTENER is RECONSTRUCTED in the child as its own socket on the
  same port, so two live listeners race for the SYN and the parent's `accept` waits for a
  connection it will never see. `addr = srv.getsockname()` is captured BEFORE the fork (it raises
  EBADF after `srv.close()`).
* `inetfork1.sh` gained post-reap `MSG_PEEK` diagnostics: they are what distinguished "the bytes
  were delivered to the wrong end" from "the bytes were lost", which is the whole difficulty of
  this bug class.
