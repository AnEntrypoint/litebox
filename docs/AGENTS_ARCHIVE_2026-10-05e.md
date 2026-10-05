# AGENTS.md archive 2026-10-05e -- verbatim pre-recompact snapshot + this pass's mechanism

Preserved verbatim below: `AGENTS.md` exactly as of `37c2374` (the fork-protection narrowing).
Section 2 afterwards records this pass: the chrD91 goal run, the `fd.rs:141` guest-reachable
panic it exposed, and the copy-on-write / narrowing fixes it validated.

---

## 2. This pass (`4964934`) -- the chrD91 panic and the EBADF close path

### 2.1 chrD91: the goal run reaches the browser and then dies on `unreachable`

chrD91 ran `chrdesk54.sh` + `-Publish 8081:8081` on the `37c2374` binary (launched at 2,606 MB
free). Chromium came up first try with its OWN sandbox (no `--no-sandbox`; `--disable-gpu`
present), `DEVTOOLS code=200` at guest t=15s, window `0xa00003 "litebox A-blue"` 800x600, guest
xwd `blue=99.79%`, CDP `shot 800x600 blue=99.78%`, host-browser frame `1354x834 blue=42.06%
root=54.19% white=0.00%`. Then, in the `.err`:

    epoll poll with socket fd: Errno(9 = EBADF)          (repeated, before the panic)
    thread '<unnamed>' panicked at litebox\src\fd\mod.rs:141:13:
        internal error: entered unreachable code
    thread 'main' panicked at ...platform_windows_userland\src\lib.rs:1371:10:
        cross-process fork child's guest-execution thread panicked

and from guest t=90s onward the in-guest probe `curl 127.0.0.1:8081` answered
`curl: (7) Failed to connect ... Could not connect to server` while the host stream held.

### 2.2 The mechanism

`fd.rs:141` was `Descriptors::close_and_duplicate_if_shared`:

```rust
let idx = fd.x.as_usize()?;
let Some(old) = self.entries[idx].take() else { unreachable!() };
```

`as_usize()` answers `Some` for any `OwnedFd` that has not been `mark_as_closed()`ed, and
`None` afterwards -- so reaching `take()` with `None` means the NUMBER OUTLIVED ITS ENTRY. Two
ways that happens, both reachable from `close(2)`: a second close racing the first (each
`close(2)` builds a fresh `OwnedFd` from the raw integer), and an fd rebuilt in a cross-process
fork child from a number whose entry was never re-created. `epoll` is what makes it likely:
it stores `TypedFd`s, and unlike Linux it holds no file reference, so once the referent is gone
every poll reports EBADF and the guest retries the close. Linux answers EBADF for a close of a
number nothing owns, and the host process IS the whole session, so this is a panic where an
errno belongs.

The three sibling `unreachable!()`s on the same path got the same treatment, because all three
are one `close(2)` away:

| site | shape | answer |
| --- | --- | --- |
| `Descriptors::remove` (`fd.rs:114`) | slot empty, nothing to remove | `None` + `warn!` |
| `close_and_duplicate_if_shared` (`fd.rs:141`) | slot empty, close answers EBADF | `None` + `warn!` |
| `Network::close` Deferred arm (`net.rs:1794`) | entry vanished between the defer decision and `consider_closed.store` | `Ok(())` + `warn!` |
| `do_close_and_replace` (`file.rs:4679`) | the raw number is consumed by no subsystem the close path enumerates | `Err(Errno::EBADF)` |

`fd_consume_raw_integer` (`fd.rs:957`) errors BEFORE it takes the slot
(`let ret = self.fd_from_raw_integer(fd)?`), so the fallthrough leaves the number occupied --
it leaks rather than killing the session, which is the right failure direction for a leak.
An early draft put `assert!(success, ...)` in that arm; that would have panicked in exactly the
branch being made safe, so it was removed.

### 2.3 What the fix bought (chrD92 / reg1e)

- `reg1e` (whole fork suite: inetfork1/2, shmfork1-3, flockx1) is byte-identical to the `reg1d`
  baseline with digits normalized -- 93 lines, 46 PASS, 0 FAIL, `REG1_DONE`.
- `chrD92` (`4964934`): **zero `panicked at`** over 1.5 MB of `.err` (logscan), `DEVTOOLS
  code=200` at t=15s, guest xwd `blue=99.79%`, CDP `shot blue=99.78%`, CDP eval
  `{"rs":"complete","bg":"rgb(32, 192, 240)","w":800,"h":600}`, and the host-browser frame
  after 60 s of stream time: `1354x834 readyState=4 blue=42.41% root[48,80,112]=53.91%
  white=0.00% lightGrey=0.00%`.
- **What it did NOT fix**: the in-guest `curl 127.0.0.1:8081` refusal from t=120 on. chrD91
  started failing at t=90 and chrD92 at t=120, both AFTER the host client's first line in the
  log, and chrD90 (`47cefb2`) answered 200 through t=180 with the host client connected from
  t=60. At the failing instant the guest's own `curl` to the UNPUBLISHED 9222 answers, selkies
  keeps encoding (`EncFPS 13.6`) and the host browser keeps receiving the frame. Carried as
  Open #1 with the chromium-free repro.

### 2.4 Harness notes this pass

- `chain92.ps1` polls `.wfgy/reg1e.out` for `REG1_DONE` and then launches the goal run, so the
  regression suite and the goal run chain without a human in the loop.
- Sweeping 52 leaked `litebox_runner_*` processes (CIM, `Stop-Process -Force -Id`) took free RAM
  from 2,596 to 3,573 MB. gm's own `agentplug-runner.exe daemon` (~1.2 GB) was left alone.
- Censusing the host-browser `<video>` in-page (canvas `drawImage` + `getImageData`) is one
  `evaluate_script` call and never puts an image in the transcript.

---

# litebox -- current state (2026-10-05d; recompact of `-05c`; verbatim `-05c` + this pass's mechanism in `docs/AGENTS_ARCHIVE_2026-10-05d.md`)

CURRENT-STATE index. Every claim carries a sha, `file:line`, or numbers. Shorthand: `process.rs`/`file.rs`/`mm.rs`/`unix.rs`/`net.rs` = `litebox_shim_linux/src/syscalls/<x>`; `platform/lib.rs` = `litebox_platform_windows_userland/src/lib.rs`; `fork.rs` = `.../process_fork.rs`; `fd.rs` = `litebox/src/fd/mod.rs`; `lazy.rs` = `.../lazy_file_map.rs`; `mm/linux.rs` = `litebox/src/mm/linux.rs`. **This file wins.**

## Where things stand

- **GOAL MET**: sandboxed chromium -- its OWN sandbox, no `--no-sandbox` -- renders VISIBLE IN A REAL HOST BROWSER (`chrdesk54.sh`). Best run chrD90 on `47cefb2`: host-browser `1354x832 blue=42.22%/root=54.11%/grey=0/white=0`; `selkies=code=200` at guest t=30/60/90/120/150/180; **ZERO `panicked at`**. Guest xwd `blue=99.79%`, CDP `captureScreenshot blue=99.78%`.
- **HEADLESS TOO (cb66 arm Z, `f345821`)**: DEFAULT config -- zygote ON, chromium's OWN sandbox, GPU process FORKED from the zygote -- `PNG_APPEARED i=6`, `800x600 #20c0f0=99.95%`, `CHROMIUM_RC=0`. `--in-process-gpu` is now only the control. A forked child's log now reads `1005/000939`, not cb52's all-zero `0100/000000`.
- **Branch `main` == `inetfix` == `7c1d987`**: inet carry + shm carry + the fd fixes + the layered-inode fix + the private-file copy-on-write fix LANDED.
- **THE SELKIES DEATH IS `47cefb2`, NOT HOST RAM**: `DescriptorEntry::as_subsystem_mut()` (`fd.rs:1095`) did `downcast_mut().unwrap()` and panicked the whole guest process (101 -> rc 137) at selkies' first post-connect fork. The chrD79-85 "ENVIRONMENTAL, host RAM" verdict is WITHDRAWN (chrD87 watchdog-ON vs chrD88 OFF, 3867MB free, panicked identically). Two selkies failure classes: last line `Could not obtain XFCE session environment` = the fork child died (survivable -- chrD90 `RC: 1`); host-RAM starvation is separate. **Check free RAM first regardless.**
- Judge a host-browser frame only after ~60s of STREAM time (chrD32 `white=42.61%` -> `blue=42.23%` at t=61s); white/grey is PRE-FIRST-PAINT. Guest time dilates 3-4.6x vs wall; attach a host browser only to take the frame.
- **Before ANY run**: sweep runners by CIM (`Get-CimInstance Win32_Process -Filter "Name like 'agentplug%' OR Name like 'litebox%'"`), close other host browsers, require >=2.9GB free. `taskkill /F /T` did NOT kill a 1013MB runner; `Stop-Process -Force -Id` did (some are undeletable but killable). A background run can be reaped by host memory pressure mid-way (cb21 lost arm L1).

## Fixed, newest first (mechanism per sha in `docs/AGENTS_ARCHIVE_*`; keep only the RULE)

- **A CARRIED SHARED REGION MUST REACH THE CHILD WITH ITS OWN PROTECTION, NOT THE WIDEST (`37c2374`).** The fork parent mapped every carried region at the widest protection the section's ceiling allows and never narrowed it, and `adopt_carried_shared` on the child side only does bookkeeping -- so a guest `PROT_READ` `MAP_SHARED` region arrived as `PAGE_EXECUTE_READWRITE`: the child could write into the object every mapper (including the parent) sees, and its own guest write there would not fault. Fix: `VirtualProtectEx` the view down to `prot_flags(carry.perms)` while the child is still suspended, the only point before its first instruction where the two can be made to agree. A narrow can still fail on the same ceiling, and a too-wide view takes nothing away, so the failure is reported, not fatal. **reg1d / cb68c / cb70c byte-identical to the pre-change baselines, zero panics.**
- **A PRIVATE FILE MAPPING IS A COPY-ON-WRITE VIEW OF ONE SHARED SECTION (`7c1d987`; closes arm F4).** One section object serves every process that maps the file, and the VIEW is copy-on-write: `VmFlags::VM_PRIVATE_FILE_COW` (bit 11) -> `write_qualifier()` -> `MemoryRegionPermissions::COPY_ON_WRITE` -> `PAGE_WRITECOPY`/`PAGE_EXECUTE_WRITECOPY` (Windows `prot_flags`), `MAP_PRIVATE` not `MAP_SHARED` (linux/macos `map_shared_memory`). So `mprotect(PROT_READ|PROT_WRITE)` on a mapping created `PROT_READ` SUCCEEDS (Linux grants `VM_MAYWRITE` at mmap time), the write is this process's own, and the 324 MB binary still stays ONE copy across ten processes. **cb68: F4/F4B/F4C/F4D/P2 `rc=0 errno=0`, write sticks, bytes preserved, parent AND cross-process fork child; CTL1 (unmapped) still ENOMEM=12; arm S (genuine `MAP_SHARED`) still shares both ways. cb70: write privacy holds in BOTH directions across a cross-process fork, `F_FILE_UNMODIFIED=True`.** RULE: **the qualifier travels with the VMA** -- `insert_mapping`, `protect_mapping`, `duplicate` and `shared_region_carry` OR it in, and the fork PARENT's `MapViewOfFile3` into the suspended child must ask `PAGE_*_WRITECOPY`, or the child's view is an ordinary writable one and its writes land in the object every mapper sees (cb70 caught exactly that: `P_SEES_FILE_NOT_CHILD_WRITE=False`). A probe whose only output is "mprotect returned 0" cannot see that. Host facts: `.wfgy/cowprobe.cs`, archive `-05d` §2.1.
- **`reset_pages` no longer `unimplemented!()`s on `madvise(MADV_DONTNEED)` over a file-backed range** (a panic there kills the whole session): a no-op that preserves the bytes -- legal, because `MADV_DONTNEED` leaves the contents UNSPECIFIED, it does not require them to change.
- **`f345821` AN INODE NUMBER MUST NAME THE FILE, NOT COUNT THE PATHS THIS PROCESS HAS STAT'ED (this is what killed the GPU process).** A cross-process fork child rebuilt its fs with an EMPTY lookup table, so it handed out 1, 2, 3, ... in touch order. **ld.so decides "already loaded" by `(st_dev, st_ino)` vs every loaded object's `l_dev`/`l_ino`, which are the PARENT's numbers** (libm=6, libz=8, libexpat=9, libc=10): cb64 watched a byte-identical COPY of libEGL.so.1 come back AS libm (handle `0x7feffffb4000`, exactly `dlopen("libm.so.6")`'s), every `dlsym` NULL, and the GPU process -- a zygote fork child that dlopens its whole GL stack -- died on `call *rax` with `rax=0` (`rip=0x0 cr2=0x0 error_code=0x14`). Fix: `layered_ino()` = FNV-1a over the underlying `(dev, ino, rdev)`, forced odd. **cb65: all 10 child dlopens name themselves, `IDENT_CHANGED_VS_PARENT=0 of 69`.** RULE: **any number a guest can observe across processes must be a function of the file, never of this process's history.**
- **A `PROT_NONE` FILE MAPPING IS A RESERVATION, NOT A SHARED READ-ONLY VIEW.** The shared read-only path carried flags **89** (no `VM_MAYWRITE`), so `protect_mapping` refused the later `mprotect(PROT_READ|PROT_WRITE)` with **EACCES -- in the parent AND in a cross-process fork child** (cb45 F2). Fix: exclude `prot.is_empty()` (**cb45b: `rc=0 errno=0`, write sticks, parent and child; F3 `PROT_NONE`->`PROT_READ` reads the file's real bytes**). The `PROT_READ` variant (F4) is the copy-on-write bullet above. `a2ac697` a `PROT_NONE` file mapping must still carry the file's bytes.
- **A CARRIED SOCKET'S RX MUST REACH THE PROCESS THAT IS READING IT.** A carried socket has TWO proxies (one per process heap) and every tick drained into whichever ran first. BOTH halves needed: (1) the shared tick skips the TCP **and** UDP rx drain when `shared && !proxy.has_observers()`; (2) `receive` (`net.rs`) calls `drain_rx_into_proxy(fd)` first when shared -- a NON-BLOCKING read registers no observer, so (1) alone starves it. **12/12 blocking + 6/6 non-blocking clean; `reg1.sh` all PASS.** Residual: both referents observing still race.
- **`47cefb2` A MISTYPED FD IS EBADF, NEVER A PANIC.** Every `TypedFd` resolver in `fd.rs` gates on `matches_subsystem::<Subsystem>()`; `with_metadata` (`fd.rs:596`) and `get_proxy` (`net.rs:1194`) too.
- `9997313` **an INET socket ACTUALLY crosses a cross-process fork** (`Network::fork_carry_spec`/`fork_adopt`, `inet:<cloexec>|<v6>|<nonblock>|<spec>`). **RULE: `FD_CLOEXEC` is about `execve()`, not `fork()`** -- the arm that dropped cloexec sockets killed every Python socket (PEP 446). Rejections log at `warn!`, so a dropped INET fd leaves ZERO evidence at default log. Named by ENDPOINTS, never smoltcp slot. **`inetfork2.sh` 8/8.**
- `6183f53` **an unplaceable page-granular hole must not kill the host process** (`reserve_and_commit` rounds MEM_RESERVE out to 64 KiB; that range is ~34 GB above the arena base -- never an arena-region bug). Fix: RELOCATE a `Hint`. Signature: `diag-place-blocked: requested range is free but its allocation granule is unavailable ... behavior=Hint`.
- `a629714` **a cross-process fork child SHARES `MAP_SHARED` with its parent**. Trap: **validate every segment BEFORE reserving the group placeholder**. **`shmexec1.sh`: anon `MAP_SHARED` survives `os.fork()+execv`, `subprocess.run`, `os.posix_spawn`, the real `xfconf-query` and `asyncio.create_subprocess_exec`** (its own "CORRUPT" verdict is BACKWARDS).
- `ae6926d` (branch `drmevdev`, NOT merged) shared DRM/evdev tables make the last two `GlobalState` registries arena-resident (~16 KiB per fork family).
- `f260226` **cross-process `flock(2)` EXCLUSION works**: a child's `LOCK_EX|LOCK_NB` on a parent-held file is EWOULDBLOCK (11). **Keyed on `(dev, path)`, NOT `(dev, ino)`**; A/B `LITEBOX_FLOCK_SHARED_OFF=1`.
- `6a472b5` **`shmat` on an `IPC_RMID`'d-but-still-attached segment SUCCEEDS** -- X11Libre's BadAccess there killed **every MIT-SHM client = every GTK app, the whole XFCE shell**. A segment dies at RMID only once `attaches == 0`.
- `aca3a53` a `write(2)` past an object's page-rounded capacity grows the one store. `9f7c994` **a sized memfd is ONE store**.
- `575f0e2` **an uncarriable fd returns NO errno -- it silently drops the fork off the cross-process path** onto the RELOCATING fallback (`Exception(14) 0x7` before one instruction). **Never run the stack with fork OFF.**
- `f5d73ff` the flock registry lives per host process, not on byte-shared `GlobalState`. **That panic correlated with LAUNCH ORDINAL, so every "X kills chromium" A/B in that window measured the ordinal, not the variable.**
- Pipes/locks/waiters: `31ca26b` `MAX_INLINE_WAITERS` 32->128; `1732ad9` a carried-pipe pump must not outlive its child; `463dc29` **never drain a pipe the guest holds for itself** (libuv's GLOBAL SIGNAL LOCK); `16f3e76` **no path holding `net_lock` may block**; `a1423ee` HOLDING is not READING; `7d2a6a7` `SHARED_UNIX_CONN_CAPACITY` 1024->4096 -- chromium renders with its own sandbox; `efce1a5` `/proc` enumerates the whole session (`guest_pids=30`; before **1**).

## Open, in rough priority order

1. **THE GOAL RUN HAS NOT BEEN RE-RUN SINCE `f345821` + the COW fix** (host has been ~2.3 GB free; the rule is >=2.9 GB). `.wfgy/chrdesk54.sh` + `-Publish 8081:8081`, judge the host-browser frame only after ~60 s of STREAM time. Both halves below it are already measured: headless cb66 arm Z (default flags, separate GPU process) paints, and windowed cb67 paints -- **cb67's own census numbers are INVALID** (the decoder read the xwd masks one field early, so the white control came out `#00f0f0`; the masks are `f[14]/f[15]/f[16]`).
2. **chrD83: 2 x `Exception(14) error_code=0x15`** = user-mode instruction fetch to a PRESENT non-executable page, distinct from the `0x7` relocating-fork signature. chrD79/85 have ZERO and chrD83 ran starved beside 16 `chrome.exe`; **re-measure on a clean host before calling it a litebox bug.** `LITEBOX_DIAG_FAULT_VQ=1` shows `PAGE_READONLY` where `PAGE_EXECUTE_READ` is expected; `prot_flags` (`platform/lib.rs`) is CORRECT. `cb9.sh` (map in parent, execute in child) and `cb19.sh` (2 generations) never run.
3. A real `flock(2)` consumer (chromium's profile lock, SQLite) -- `f260226` is verified by `.wfgy/flockx1.sh` only.
4. Host memory is the binding constraint, not a litebox bug. **Check host RAM FIRST.** `LITEBOX_DIAG_ALLOC_STACK=1`, `_MEM_BREAKDOWN=1`; `memsamp.ps1`, `wsmap.ps1 -ProcId`. **Snapshot-binary runners leak: sweep by CIM, not by name.**
5. **The 128MiB shared kernel arena is nearly full from ~6 concurrent `GlobalState`s** (each 15-21MB, dominated by `SharedUnixConnTable`): `arena exhausted` hits ~1 run in 5 at BASELINE -- size any new arena-resident table against that.
6. Verify in the full stack (committed, never seen in a browser): `3e1ef47` SCM_RIGHTS over a cross-process unix connection; `b012910` lazy file map stale entries; `08ae94f` `/proc/<pid>/fd`. Repro `.wfgy/th1.ps1 -Run th1|th2|th3`.
7. Native kernel-COW fork on Windows (`.gm/prd.yml` `native-kernel-cow-fork`); writable layer shared across processes; AF_UNIX exhaustion still silent (256 slots, keys >108 bytes); `timerfd`/`signalfd` uncarriable.
8. Chromium startup flakiness: chrD56 rc=0 with `crashpad ... mkdir .../Crash Reports: No such file or directory (2)`, never mapped a window -- `homemk.sh`: a parent-made dir chain IS visible to a `setpriv --reuid 911` child 30/30, so timing, not visibility.

## How to run and drive it (harness lessons that cost sessions)

- **No WSL, no hypervisor, ever.** Native Win32 exe; host tooling PowerShell or Git Bash; launches `CREATE_NO_WINDOW`.
- Cheap repro: `target/release/litebox_runner_linux_on_windows_userland.exe -Z --oci-image docker.io/library/debian:stable-slim -- /bin/bash -c '<script>'`. PowerShell never Git Bash for guest paths; single quotes only inside `-c`. **Build ONLY `cargo build --release -p litebox_runner_linux_on_windows_userland`. Never pipe a build through `tail`.**
- Full stack: `.wfgy/pass118_full.ps1 -Run <name> -MaxSeconds N`. Cheap repro: `.wfgy/guest2.ps1 -Script <sh> -Run <name> -Secs <n>` (script on STDIN to a guest `bash -s`); sets `LITEBOX_PROCESS_FORK=1`, `_LAZY_FILE_MAP=1`, `_OCI_USE_LAST_RESOLVED=1`; `-ExtraEnv`, `-Publish`, `-Exe`. **`rc=137` at the END of a run is the harness cap, not a bug.** A background launch reporting "exit code 128" is usually a lie -- `.wfgy/<run>.out` is authoritative.
- **Chromium does NOT exit after a failed `--screenshot`**: `wait` blocks to the harness cap and STARVES later arms (cb18 lost B/C, cb21 lost L2-L4). **KILL the arm (`kill -9 $ARM; pkill -9 -x chromium`)** and keep a capture's deadline BELOW the PNG poll window. **`timeout(1)` HANGS in-guest -- it is NOT a bound**; bound with `-Secs` or an in-guest background+wait loop, and never conclude "X hangs" from a cap-killed run without per-step BEFORE/AFTER prints.
- **`$?` after a pipeline is the LAST command's status**; **an unquoted heredoc expands `${PIPESTATUS[0]}`/`$(date +%s)` at FILE-WRITE time**; **a nested heredoc terminator (`PYEOF` inside `PYEOF`) hangs the script -- write probe files with the Write tool**; **Bash heredocs collapse `\\` -> `\`**, build Python escapes with `chr(92)`. **Guest scripts must be LF** -- count CR bytes in python, NOT `grep -c $'\r'` (false 187 for a file with 0 CR). **A probe that cannot fail is not evidence** -- include a negative control. `socket.timeout` has `errno=None`, so `"errno=%d"` raises TypeError and the traceback REPLACES the verdict (use `%s`).
- Cargo cannot relink while a runner holds the exe (`os error 5`). Kill every runner first; if the lock persists **rename** the exe, build, delete `.prev*`. **Check the binary is NEWER than the commit under test.** In PowerShell `$?` is False after a successful `cargo build` whose stderr was redirected -- read the log's last line. Unkillable runner holding the boot lock: `Remove-Item -Force target/release/.litebox-cache/boot.lock`.
- `rc=137` from a fork child does NOT prove a kill: `decode_cross_process_exit_status` (`process.rs:390-403`) falls back to SIGKILL for ANY exit with no marker. `.err.log` SIZE is run health; `chrD*.err.log` is BINARY to gm `codesearch` -- use `.wfgy/logscan.py <file> <term>`. **Never dump a raw image into the transcript** (a 1354x832 PNG data URL overflowed the tool result) -- compute the census in-page/in-guest.
- **Guest probe constraints**: there is **NO C compiler in the image** (`gcc` -> rc 127), so every probe must be python; `dlopen("/usr/lib/chromium/chromium")` fails anyway ("cannot dynamically load position-independent executable"). **`python3` in a `guest2.ps1` run is itself a cross-process fork child** (`pid=3 ppid=1`) -- bash forks to exec it -- so a probe's "generation 0" is already one fork deep. `readelf --dyn-syms` TRUNCATES long names to `[...]` and versioned symbols carry `@`; use `readelf -W`, strip `@`, skip `[`. **`LITEBOX_*` are host-side runner vars -- invisible in-guest, so in-guest A/B of them is impossible.**
- Diagnostics: `cdb -pv -p <pid> -xd av -xd sse -c "~*kb 25; qd"` (NOT on PATH); `litebox_diag::{process_timeline,socket_read,unix_conn_teardown,stderr_capture}=debug`. The image's Xvfb is XLibre and ABORTS when `/tmp/.X11-unix` exists with the wrong mode -- `mkdir -p /tmp/.X11-unix && chmod 1777` BEFORE Xvfb; dbus-daemon non-forking; use `xfce4-session`. **A pipe the guest holds for itself must be probed with a zero-timeout `select()`, never `read`**; asyncio closes a child stdin the instant it exits -- `subprocess.Popen` and keep `p.stdin`.
- Repo hygiene: tars, frame dumps, debug logs never in git (`.wfgy/` ignored); **commit as lanmower only, no `Co-Authored-By`**; **never `git push` to `origin` (github.com/AnEntrypoint/litebox) without per-instance confirmation.** `4792fc5` pins `*.rs`/`*.toml`/`*.md`/`*.lock` to LF -- a wholesale CRLF flip blew `git diff` up to ~27k lines.

## Standing lessons and hard constraints

- Guest-reachable code returns an errno, never a panic (the host process IS the whole session). Refusal errno is contract: EPERM degrades, EINVAL/ENOSYS fails hard.
- `LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check. Never subtract timestamps across a parent and a fork-child log (`init_logging()` resets elapsed time per child).
- **A `TypedFd` index is valid only against the `Descriptors` that inserted it, and only for its own subsystem** (`fd.rs:1095`, `net.rs:1194`). Shared-memory structs must hold no process-relative pointers. **A per-process object (a socket proxy) cannot be the delivery target for another process's data.** A `SharedUnixAddrPresenceTable`-shaped mutable table needs every write path mirrored to the shared side.
- **`(dev, ino)` is cross-process-stable for IMAGE files ONLY (`f345821`)**, never for runtime-created ones (a fork child's `InodeAllocator` renumbers: `dev` equal, `ino` 527 vs 1). **Keep keying shared registries on `(dev, path)` (`FilesState::lookup_fd_path`)** -- `f260226`'s flock table depends on it. cb69: image files `IMAGE_MATCH=YES`; runtime files `RUNTIME_MATCH=0 of 4` but `CHILD_COLLISIONS=0 of 9`, and the parent sees a child-created file with the SAME number after the writable-layer import -- a wrong number is silent, never a crash.
- Cross-process fork (`LITEBOX_PROCESS_FORK=1`): carried = pipes/regular files/eventfds/pty/unix sockets + INET (`9997313`) + `MAP_SHARED` (`a629714`); dropped = pty fds and non-INET cloexec fds. Slots (`CROSS_PROCESS_FORK_SLOT_COUNT` 6) gate the spawn and fail open after 8s; keyed by child host pid, freed at STARTUP-COMPLETE not at reap (`a24ce9d`). The env var is PRESENCE-CHECKED (`var_os`): `=0` still ENABLES it. The post-duplication clone site is SUPERSEDED -- believe `try_cross_process_fork`'s log.
- **Lazy file map**: chunks >= `MIN_LAZY_LEN` (256 KiB) are armed `PAGE_NOACCESS` and VEH-filled from a `'static` source slice keyed by OCI LAYER INDEX (`register_layer_sources`), so sources are identical in every process of a fork tree (wrong-source EXCLUDED). **A guest reads guest strings through a bare host-side dereference with NO validation** -- `Some("")` in a syscall trail means a real 0x00 byte, `None` means the read faulted.
- **`ps -eo pid,args` is INVALID for chromium's children -- every chromium process reports `--type=zygote`**; `/proc/<pid>/cmdline` and `comm` are EMPTY; `/proc/self/maps` can be EMPTY in a fork child (cb36: parent 108 lines, child 0) while `/proc/self/exe` works. Judge liveness by CDP or pixels regardless. **This is also why selkies' `_pids_of("xfce4-session")` always falls back.**
- `chroot(2)` is real (`e608959`): `FsState.root` is shared by `CLONE_FS`; `cwd` is root-space, dirfd-relative paths stay unrooted. Do NOT test `CLONE_FS` with a raw `clone(CLONE_VM|CLONE_VFORK|CLONE_FS)` from CPython (dies 139); use pthreads.
- Logs: default `warn,...fork_verify=error`. Verbosity from `LITEBOX_LOG`, not `RUST_LOG`. `litebox_util_log::warn!` takes a single literal with inline captures -- **no trailing format args, no `\`-continuation inside the literal**.
- Env: `LITEBOX_PROCESS_FORK`, `_LAZY_FILE_MAP`, `_IDLE_TRIM=0`, `_OCI_USE_LAST_RESOLVED`, `_DUMP_FRAMES`, `_PROCESS_FORK_IGNORE_FDS`, `_DIAG_FORK_SHARED_FORCE_FAIL`, `_FLOCK_SHARED_OFF`; diagnostics `LITEBOX_DIAG_LOCKSTALL`, `_WAIT_DUR`, `_LOCKSTALL_TRACE`, `_ALLOC_STACK`, `_MEM_BREAKDOWN`, `_FAULT`, `_WATCHDOG`, `_NO_FAULT_WATCHDOG`, `_NO_EXTERNAL_FAULT_WATCHDOG`, `_SOCKET_READ_TARGET`, `_FAULT_VQ`.
- Subagents: Sonnet, not Opus. Web search: Google, not DDG (camoufox if blocked).

## Child exit: how a parent learns (and how it can fail)

`arm_cross_process_exit_notifier` blocks a host thread on the child's INITIATING THREAD handle, pushes SIGCHLD into `process.shared_pending` + `interrupt_all_threads()`; `sys_wait4` re-polls every 15ms and runs `import_cross_process_writable_layer` on reap. **`waitpid(-1)` only sees entries registered in THIS host process -> ECHILD.** **SIGCHLD is dispatched only when the thread passes `check_for_interrupt`/`prepare_to_run_guest` (`wait.rs:41-64`)**, so glib-style SIGCHLD reaping can miss it; `signalfd` never wakes (`signalfd.rs:185-191`); `pidfd_open` (`inotify.rs:353`) does. **The external fault watchdog `TerminateProcess`es a process whose CPU delta stayed <=10ms for 15s** (armed only by `mark_fault_terminate_armed()`, `platform/lib.rs:2338`/`:2642`) -- NOT the selkies killer.

## Other subsystems (Linux runner / OCI, shared memory and locks, file visibility)

Moved verbatim to `docs/AGENTS_ARCHIVE_2026-10-05b.md` section 10 to keep this file under 30 kb. The one-line rules that still cost sessions: a file one host process wrote is invisible to siblings until it exits; a fork child gets the parent's writable layer AT SPAWN and its writes reach the parent only on `wait4`; extend `SPILLED_PREFIXES` rather than adding `/tmp/`.

## Closed -- do not re-attempt without a genuinely new approach

**"`--screenshot` completes only with `--in-process-gpu`" / "the forked GPU process dies"** -- `f345821`, cb66 arm Z. **"a fork child's `dlopen` returns the wrong library"** -- `f345821`, cb64. **"a `PROT_READ` private file mapping cannot be `mprotect`ed writable"** -- the copy-on-write view fix, cb68 F4. **"a `MAP_PRIVATE` write is visible to another process"** -- cb70 (the fork parent must map the carried view `PAGE_*_WRITECOPY`). **"the compositor presents nothing"** -- `9997313`. **"a fork child does not share `MAP_SHARED`"** -- `a629714`. **"selkies dies when a browser client connects"** -- `47cefb2`, chrD90. **"a carried TCP connection loses a message"** -- the rx-delivery fix. **"the GPU process is blocked by chromium's sandbox"** -- cb21 L1: it loads zero libraries. **"a library's bytes are wrong"** -- cb14/cb15/cb37/cb38/cb39. **"a fork child cannot dlopen/dlsym"** -- cb30/cb31/cb33. **"a fork child's anonymous/heap memory is zeroed"** -- cb32. **"a fork child's inherited pointers are stale"** -- cb36/cb37. **"dconf/NSS `nspr_use_zone_allocator` misses matter"** -- cb31: those symbols are genuinely ABSENT from `.dynsym`. Also: net_lock freeze (`16f3e76`); "spawning `xset` wedges the loop" (`463dc29`); "selkies stalls at startup" (8080/8081 + basic auth with no password); fork admission cap 6->3 REVERTED; fd-number aliasing; "a parent-made dir is invisible to a fork child" (`homemk.sh` 30/30); chrD53-56 grey (ENVIRONMENTAL).

## Docs and tooling map

Archives newest first under `docs/`: **`..._2026-10-05d.md` (this pass -- verbatim `AGENTS.md` as of `7c1d987` + the copy-on-write mechanism and the `cowprobe` host facts)**, **`..._2026-10-05c.md`** (the GPU-process hunt cb40-cb66 and the layered-inode cause), `..._2026-10-05a.md` (cb40/cb41: lazy bind and cold pages both refuted), `-04j.md` (cb14-cb18 fidelity, `LD_DEBUG=bindings`, cb21 L1), `-04i/-04h/-04g/-04f/-04c.md`, the `-10-0*`/`_2026-09-*` set, `fork-region-grouping-design.md`, `track-b-fork-fix-progress.md`, `veh-exception-handler-design.md`, `advisor/ADVISORY-00{1,2}*.md`.
`.wfgy/` (git-IGNORED, so `codesearch` never sees it): host tooling `logscan.py`, `lockstall.py`, `scan_waiters.py`, `symstk.py`, `guest2.ps1`, `pass118_full.ps1`, `framecensus.py`, `memsamp.ps1`, `wsmap.ps1`, `th1.ps1`. Probes worth knowing: **`reg1.sh` (whole fork suite in one run: inetfork1/2, shmfork1-3, flockx1)**, `shmexec1.sh`, `homemk.sh`, **`cb65.sh`** (layered-inode acceptance), **`cb66.sh`** (THE goal test: default config, zygote ON), **`cb68.sh`** (mprotect matrix + controls), **`cb70.sh`** (`MAP_PRIVATE` write privacy), **`chrdesk54.sh`** (THE goal recipe), `apps1/apps3.sh`, `chrshot4/8.sh`; the GPU hunt's cb14-cb64 and the cb40-cb49 set are listed verbatim in archive `-05d` §1.
- gm `codesearch` (INVARIANT 4: never grep/find): `mode: "literal"` is exhaustive; `mode: "dual"` is a ranked SAMPLE, so never conclude "absent" from it. Scope with `path`/`glob` -- an unscoped literal scan caps at 40 matches and is therefore NOT exhaustive in practice.

## Guest stack: chromium + selkies

- **TWO chromium failure modes -- do not merge.** (a) `Crashing due to FD ownership violation:` + `zygote_host_impl_linux.cc:129] No usable sandbox!` = the `CanCreateProcessInNewUserNS()` probe. (b) rc=133 (SIGTRAP) with NO sandbox line = crashpad/HOME.
- **A `HOME` chromium shares with anything root ran kills it** (root-run app makes `$HOME/.config` 0755 root -> uid 911 gets `stat .../Crash Reports: Permission denied (13)`; crashpad mkdirs only its LAST component). Fix: `mkdir -p $HOME/.config/chromium; chmod -R 777; chown -R 911:911` on a HOME nothing else touched -- `chmod 777` on a SHARED HOME is NOT enough.
- NON-ROOT (`setpriv --reuid 911 --regid 911 --init-groups`); `--user-data-dir` `chmod 777` when a root shell created it (else `SingletonLock: Permission denied`, exit 21). `HOME=/tmp/cuhome` exits 0 (`cpad1.sh`); `--crash-dumps-dir` does not exist in Debian's build.
- `seccomp(2)` is real: `syscalls/seccomp.rs` interprets classic BPF at the top of `Task::do_syscall`; `SECCOMP_RET_TRAP` must RETURN the syscall number (`ad2659f`), payload in `si_errno` (`067367b`).
- Still open: (1) the launcher thread SERIALISES cross-process spawns, so children hit the 15s "no connection" self-termination; (2) `Vmem::duplicate` skips `VM_OWN_FORK_PADDING`; (3) ~1/run `rebuilding a carried SCM_RIGHTS fd failed errno=ENOENT`; (4) two simultaneous chromiums are unaffordable -- run ONE.
- **CDP from inside the guest is the decisive chromium probe** (`chrdesk38.sh`, `/tmp/cdp.py`): `GET http://127.0.0.1:9222/json/list` (`--remote-debugging-port=9222 --remote-allow-origins=*`), then a RAW stdlib websocket for `Page.enable`, `Runtime.evaluate`, `Page.captureScreenshot`; decode in-guest with `zlib` + the five filter types. It is chromium's OWN compositor output -- **and it TIMES OUT when the compositor presents nothing, so use `fromSurface:false`.** Never `--disable-dev-shm-usage`.
- **The client connect is what makes selkies fork**: `INFO:ws:DPI changed from 96 to 120` -> `display_utils.py:1858` -> `asyncio.create_subprocess_exec("xfconf-query",...)` (selkies runs under `/lsiopy`, NOT system python3). In chrD90 it COMPLETES with `RC: 1` (benign) and the stream holds.
- **XWD grab: decode with the MASKS, never by byte position** (`f[14]`/`f[15]`/`f[16]` = r/g/b; pixels start at `max(len(raw) - w*h*bpp, hsize + ncolors*12)`; a GTK app's 10x10 leader window cannot be grabbed). **Every `blue=0.00%` from chrD19 and earlier is INVALID.**
- **The selkies client needs its `Play Stream` button pressed or the `<video>` never gets a track** (`readyState=0`). The a11y snapshot LIES; the real element is the first `<button>` matching `/play stream/i`, and `evaluate_script` `b.click()` works where `click` on the uid times out (`readyState=4 1354x832` in ~2.5s).
- **A file a fork child wrote is visible only to the shell that reaps it**: a headless `--screenshot` must run chromium in the FOREGROUND of its subshell with the analysing python after it in that SAME subshell (`chrdesk8.sh`). Blue PNG = compositor fine; white PNG = no frame.
- Never `--enable-logging=stderr --v=1` in a long run (chrD13 stopped painting at 32-37fps). Windowed recipe: `xfconf-query -c xfwm4 -p /general/use_compositing -s false`, `xsetroot -solid "#305070"`; xfce4-session takes ~120s to register its WM.
- SELKIES 2.0.0 BINDS 8080, NOT 8081; `CUSTOM_WS_PORT` is a 1.x name it ignores. It ENABLES BASIC AUTH BY DEFAULT and with no password STOPS ITS OWN SERVER -- always `--enable-basic-auth=false`. **A host browser reaches a guest SERVER only through `-p/--publish h:g`** (OUTBOUND NAT only).
- **selkies' capture REQUIRES MIT-SHM at the X server** (hard-fails on `shm_query_version`). The client's `FATAL: ... video pipeline did not start` is diagnosed by the line ABOVE it, `Failed to start capture for 'primary': <e>`. **A log selkies writes under `/tmp/` is INVISIBLE to the launching shell -- pipe it.** A/B on ONE port is useless (`kill -9` on a wedged cross-process child does not reap it -> `Address already in use`).

## Apps: verified to paint (apps1/apps3 on `47cefb2`, zero panics)

Measured by xwd with the mask-aware decoder: `xterm` red `255,0,0` 97.4%, `xedit` white 94.2%, `xcalc` 83/17, `xeyes` 155 colours, `xclock` 188, **`mousepad` (GTK3) white 87.4%**, **`thunar` (GTK3) white 60.9%**, chromium `--app` (xwd `blue=99.79%`, CDP `99.78%`). Absent (untestable, not broken): `gedit`, `evince`, `pcmanfm`, `firefox-esr`, `galculator`, `leafpad`, `import`/`convert`; present, untested: `xfce4-terminal`. Probes `apps1.sh` / `apps3.sh`.
