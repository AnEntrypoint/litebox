# AGENTS_ARCHIVE_2026-10-04h -- mechanism prose moved out of AGENTS.md in pass `-04h`

AGENTS.md keeps one rule line per sha; the mechanism that justifies it lives here. Newest first.
This pass's compaction also REMOVED a false entry: the `-04g` claim that the selkies-client-connect
death was ENVIRONMENTAL with host free RAM as the discriminator. See "chrD87-90" below -- the cause
was a litebox fd-subsystem panic, fixed by `47cefb2`.

---

## `47cefb2` -- a mistyped fd is EBADF, never a panic (the selkies killer)

**Symptom.** Every full-stack run died a few seconds after a host browser connected to selkies.
Guest shell reported `Killed` / rc=137; the last selkies line was always
`WARNING:display:Could not obtain XFCE session environment. Falling back to direct execution.`

**Cause.** `DescriptorEntry::as_subsystem_mut()` (`litebox/src/fd/mod.rs:1095`) ended in
`downcast_mut().unwrap()`. The descriptor table is ONE `Vec<DescriptorEntry>` shared by every
subsystem, and a `TypedFd<Subsystem>` is only a `u64` index into it. When an index that names, say,
a file entry is resolved through the socket subsystem, the `TypeId` check inside `downcast_mut`
fails and returns `None`; the `unwrap()` then panicked the host process, which IS the whole guest
session. The guest saw its parent process vanish with no errno.

**Why it fired exactly at connect.** The client connect is what makes selkies fork:
`INFO:ws:DPI changed from 96 to 120` -> `display_utils.py:1858` `_run_xfconf` ->
`await subprocess.create_subprocess_exec("xfconf-query", ..., "/Xft/DPI", "-s", "120")`.
That fork child re-derives its descriptor table from the parent's exported state, which is where
the mismatched index arose.

**Second, same class, one layer out.** `Descriptors::with_metadata` (`fd.rs:596`) checks metadata
presence but NOT the subsystem, so a stale or recycled index produces
`MetadataError::NoSuchMetadata`. `GlobalStateHandle::get_proxy` (`net.rs:1194`) treated that as
`unreachable!()` and panicked a fork child (chrD89). It now returns EBADF plus a `warn!`.

**Fix shape.** Every `TypedFd` resolver in `fd.rs` gates on `matches_subsystem::<Subsystem>()`
and returns `None` on mismatch: `iter_mut`, `iter_mut_nowait`, `with_entry`, `with_entry_mut`,
`entry_handle` (the only `EntryHandle(Arc::clone(..))` construction site), `get_entry`,
`get_entry_mut`. This is behaviour-preserving: `matches_subsystem` IS the `TypeId` equality the
downcast was already testing -- the only change is that a mismatch is now an `Option`/errno
instead of a panic.

**chrD87-90 A/B (how host RAM and the watchdog were excluded).**
- chrD87 watchdog ON vs chrD88 watchdog OFF with 3867 MB free: **identical panic**.
- The arm event is created NON-SIGNALLED (`process_fork.rs:5554-5562`); the only two
  `mark_fault_terminate_armed()` callers are the VEH self-terminate paths
  (`platform/lib.rs:2338`, `:2642`); `unrecov-av-terminate` appears in ZERO logs.
- chrD90 on `47cefb2`: zero `panicked at`; the DPI child ran and returned
  `Failed to set XFCE DPI using xfconf-query. RC: 1` (benign -- no xfconf daemon in the image)
  and the stream held `code=200` at t=30/60/90/120/150/180 with
  `blue=42.22%/root=54.11%/grey=0/white=0`.

**Regression suite (`.wfgy/reg1.sh`) re-run on `47cefb2`**: inetfork1 all PASS, inetfork2 8/8,
shmfork1 12/12, shmfork2 15/15, shmfork3 11/11, flockx1 PASS (child `LOCK_EX|LOCK_NB` -> errno 11;
blocking `LOCK_EX` acquired after 3.9 s). No regression.

---

## `9997313` -- an INET socket actually crosses a cross-process fork

Spec format `inet:<cloexec>|<v6>|<nonblock>|<net spec>`, produced by `Network::fork_carry_spec`
and consumed by `fork_adopt` -> `initialize_socket` -> `insert_raw_fd` ->
`sys_dup(raw, Some(target_fd))` + `sys_close(raw)` in `install_inet_at_fd`
(`litebox_shim_linux/src/syscalls/file.rs:9778-9882`).

`6c63d41` was a half fix with three silent faults:
1. writer/reader field-order mismatch -> the parse returned `None` -> EBADF;
2. the `FD_CLOEXEC` arm sat ABOVE the INET arm, so **every Python socket was dropped** -- CPython
   creates sockets with `SOCK_CLOEXEC` (PEP 446). `FD_CLOEXEC` is about `execve()`, not `fork()`;
3. a carried connected UDP socket lost its shim-side peer, so sends went nowhere.

A dropped INET fd is invisible to the guest except as EBADF and leaves ZERO evidence at the
default log level, which is why `install_inet_at_fd` rejections now log at `warn!`.

Naming: always by ENDPOINTS, never by smoltcp slot; a carried connected or bound UDP socket is
`borrowed`. `inetfork1.sh` 8/8. `inetfork2.sh` 8/8 proves the lifetime rule: a child that inherits
a carried INET fd and then EXITS, EXECS or CLOSES it does not destroy the parent's listener or an
accepted connection.

---

## `6183f53` (= `bfc33f7`) -- an unplaceable page-granular hole must not kill the host process

`reserve_and_commit` rounds MEM_RESERVE out to 64 KiB, so any page-granular allocation poisons its
64 KiB granule for the lifetime of the process. A fork child is full of page-granular allocations,
and the first ordinary guest `mmap()` inside one died on the `assert!` in
`process_memory_range_by_regions` (`platform/lib.rs:8366`) -- chrD72 lost all three chromium
launches this way. The offending range sat ~34 GB ABOVE the arena base, which is how we know it
was never an arena-region bug. Fix: RELOCATE a `Hint` rather than failing the reservation.

---

## `a629714` (= `e41cafa9`) -- a cross-process fork child shares `MAP_SHARED` with its parent

Mechanism: `MapViewOfFile3` the parent's section into a placeholder reserved in the child.

Trap that cost a full run: **validate every segment BEFORE reserving the group placeholder.**
Checking after leaves a live placeholder sitting over the byte-copy fallback's span, which yields
487 -> `spawn/resume failed` -> `Exception(14) error_code=0x7`.

Synthetic probes never exercise a FAILED carry, so degradation is measured with
`LITEBOX_DIAG_FORK_SHARED_FORCE_FAIL=1`.

`shmexec1.sh`: a parent's anonymous `MAP_SHARED` survives `os.fork()+execv`, `subprocess.run`,
`os.posix_spawn`, the real `xfconf-query`, and the exact `asyncio.create_subprocess_exec` path
selkies uses. Note that shmexec1's own "CORRUPT" verdict is BACKWARDS -- the parent SHOULD see the
child's bytes.

---

## `6a472b5` -- `shmat` on an `IPC_RMID`'d segment that still has an attach succeeds

`mm.rs`: the refusal condition is `seg.removed && seg.attaches == 0`. X11Libre's `ProcShmAttach`
returns BadAccess where Linux's `do_shmat` has no SHM_DEST check, so every MIT-SHM client -- every
GTK app and therefore the whole XFCE shell -- died on this. A segment dies at RMID only once
`attaches == 0`; until `shmctl(RMID)` it lives (`b6e1142` + `1503276`), backed by
`<spill>/sysvshm/<name>.bin`.

---

## `575f0e2` / `f5d73ff` -- carry rules for fds and for per-process registries

`575f0e2`: an fd with no name AND no shared object yet is carried as a byte snapshot. The trap is
that an uncarriable fd returns NO errno -- it silently drops the fork off the cross-process path
onto the RELOCATING fallback, which faults the child before it executes one instruction
(`Exception(14) error_code=0x7`). Consequence: never run the stack with fork OFF, because then
every child dies, controls included.

`f5d73ff`: the flock(2) registry lives per host process, not on byte-shared `GlobalState`. A fork
child that inherited inline bytes read garbage `BTreeMap` nodes (`btree/node.rs:1232`, rc=137).
That panic correlated with LAUNCH ORDINAL, so every "X kills chromium" A/B run inside that window
measured the ordinal, not the variable.

---

## chrshot4 -- the headless `--screenshot` measurement (open item 1)

`.wfgy/chrshot4.sh` on `47cefb2`, bound by an in-guest poll loop (no `timeout(1)`), 600 s cap:

```
[s] PAGE_CHECK code=200 size=110
[s] H2_HEADLESS_SHOT
[s] H2_BEFORE pid=19
[s] POLL_EXHAUSTED i=100 (chromium still alive, no PNG)
```

200 s of polling, chromium alive, no PNG written, then the `wait` blocked until the cap. This is a
real bound, unlike chrS6's `rc=124` which was the in-guest `timeout(1)` artifact and proved
nothing. The same binary paints in a window (xwd `blue=99.79%`, CDP `99.78%`). H3 (1354x832)
never reached.

---

## apps1/apps3 -- the app paint census

`.wfgy/apps1.sh` (first pass) and `.wfgy/apps3.sh` (mask-aware decoder). Both: no `timeout(1)`, no
xfce4-session, Xvfb :1 1280x800x24 with MIT-SHM, `/tmp/.X11-unix` mode 1777 before Xvfb, census
computed in the guest, numbers only.

| app | result |
|---|---|
| `xterm -bg '#ff0000'` | `255,0,0` 97.36% |
| `xedit` | white 94.2% |
| `xcalc` | 2 colours, 83/17 |
| `xeyes` | 155 colours |
| `xclock` | 188 colours |
| `mousepad` (GTK3) | white 87.4%, grey246 7.2% |
| `thunar` (GTK3) | 1855 colours, white 60.9% |
| chromium `--app` | xwd `blue=99.79%`, CDP `blue=99.78%` |

Zero panics in both runs. Two artifacts, neither an app failure:
- apps1 read bytes `[o],[o+1],[o+2]` as RGB, but an XWD pixel word is little-endian with masks
  `00ff0000/0000ff00/000000ff`, i.e. B,G,R in memory. So it reported xterm's red as `0,0,255` and
  chromium's `#20c0f0` as `240,192,32` -- both apps were painting correctly.
- `xwd -id` on mousepad's 10x10 leader window returns `BadMatch` (`X_GetImage`); that was apps1's
  "mousepad failed". The named top-level window (`"Untitled 1 - Mousepad"` 640x480) grabs fine, and
  `-root` is always viewable as ground truth.

Absent from the image (untestable, not broken): `gedit`, `evince`, `pcmanfm`, `firefox-esr`,
`galculator`, `leafpad`, `import`/`convert`. Present and untested: `xfce4-terminal`.

---

## Harness artifacts recorded in this pass

- `chrdesk53.sh:69` literally contains `rc=\x00 wall=\x0f91123764` -- `${PIPESTATUS[0]}` and
  `$(date +%s)` expanded at FILE-WRITE time inside an unquoted heredoc. `chrdesk54.sh` replaces it
  with `( { selkies ...; echo "[s] SELKIES_RC=$?"; } | sed 's/^/[selk] /' ) &`.
- `grep -c $'\r'` reported 187 CRLF for a file with 0 CR bytes. Count CR bytes in python instead.
- A 1013 MB `agentplug-runner.exe` survived `taskkill /F /T`; `Get-CimInstance Win32_Process
  -Filter "Name like 'agentplug%'"` + `Stop-Process -Force -Id` removed it.
- `litebox_util_log::warn!` accepts `(fields; literal)` or `(literal)` with inline captures only --
  no trailing format args and no `\`-continuation inside the literal (`macros.rs:40-57`).
- `TypedFd.x` is private; only `pub(crate) fn as_internal_fd()` exists, so an fd NUMBER cannot be
  logged from `litebox_shim_linux`.
