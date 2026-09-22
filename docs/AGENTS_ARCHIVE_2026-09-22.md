# litebox archive — 2026-09-22 (26th-37th passes, drained from AGENTS.md's 2026-09-22 compaction)

This file holds the full pass-by-pass narrative AGENTS.md's own 2026-09-22 compaction pass drained
out of the live file (which had grown back over the 30KB threshold, per the standing "when
AGENTS.md exceeds 30KB, compact it" rule). Nothing here was re-derived; every paragraph below is
the verbatim text that used to live directly in AGENTS.md. Read `AGENTS.md` itself first for the
current-state summary and pointers; this file is detail-on-demand, never a starting point.

## Cross-process fork (`LITEBOX_PROCESS_FORK=1`) — full pass narrative

**Passes 4-25 (2026-09-17/20), all FIXED/REFUTED, live-verified — full narrative: `docs/
AGENTS_ARCHIVE_2026-09-17.md`/`_2026-09-18.md`.** Landed: nginx pipe-EOF handle
allow-list; `Network::socket_set`/`LocalPortAllocator`/`closing_in_background` made shared-arena
fixed arrays; `RawMutex` poison-on-dead-holder scheme; by-name Xvfb/dbus-daemon fork exclusion
relaxed; shared cross-process AF_UNIX connection plane (`SharedUnixConnTable`/
`SharedUnixConnectQueue`) designed+implemented+leak-fixed; a platform-wide smoltcp
stale-`SocketHandle` panic fixed via `catch_unwind`+`force_reset_network_after_panic()`;
`webtop_stack.sh`'s `[ -S "$XSOCK" ]`→`-e` fix; `xset q`'s silent kill fixed (`memfds`/
`shared_files` per-process-shadowed); a `GlobalState` field audit (`unix_addr_table`/
`fifo_registry` per-process-shadowed, `sysv_shm` fixed array); `sys_wait4`'s `pid==-1`/`pid>0`
no-repoll gaps both fixed; the `$XSOCK` stall closed for good via `SharedUnixAddrPresenceTable`
consulted on `ENOENT`. REFUTED along the way (recorded so they are never re-tried): the
`wait_on_tun` two-holder-deadlock theory, `has_pending`/queue logic as the ppoll-stall cause, the
AF_UNIX rendezvous "livelock" theory (24th pass: live per-request instrumentation proved the
mechanism itself sound, one real connect per boot, ~23ms, no timing/address mismatch). Still open
from this range: `SafeZoneAllocator::dealloc`'s spinlock livelock (no dead-holder recovery, unlike
`RawMutex`); `pty_registry`/`daemon_pty_masters`/`flock_registry`/`drm`/`evdev` need a deeper
redesign than a flat fixed array (non-POD payload) — the `pty_registry`/`daemon_pty_masters` slice
is now DONE, 36th pass (`syscalls::pty::SharedPtyTable`), `flock_registry`/`drm`/`evdev` still
open; `UnixInitStream::check_io_events`'s static `Init`-state report (unconfirmed).

**Twenty-sixth pass (2026-09-20)** — FIXED `EpollFile::repoll_stdin_and_timerfd_interests` reading
`is_still_ready` (always false for `EPOLLET`), not `event.is_some()`; `XVFB_UP` printed for
the first time ever. **Twenty-seventh pass (2026-09-20)** — surgical pre-create of `/tmp/empty`/
`config/`/`config/.Xresources` into `webtop_seed.tar` landed; a periodic writable-layer-sync-thread
alternative was tried, PROVEN to regress `NGINX_SUPERVISOR`, fully removed (do not re-add without
an import-lock). **Twenty-eighth pass (2026-09-21)** — `SharedFilePublishTable`
(`litebox_shim_linux/src/syscalls/file.rs`) closed `DBUS_FAILED` for good; five further live-caught
`Network`/`fd` cross-process stale-index bugs fixed one at a time (`close`/`close_handle`'s
unguarded `socket_set` touches, `queued_for_closure` fixed-array conversion + `None`-tolerant
lookups, `LocalPortAllocator`'s `unreachable!()`s, a full `net/mod.rs` `socket_set` guard sweep,
`drain_entries_full_covered_by`'s `matches_subsystem` guard) — two consecutive full release-binary
boots ran panic-free to `DE_LAUNCHED` for the first time ever. **Twenty-ninth pass (2026-09-21)** —
three more real bugs fixed live (`sys_execve`'s `copy_vector` reading `argv`/`envp` after
`end_fork_child_verification()` already cleared the translation map it needs,
`litebox_shim_linux/src/syscalls/process.rs`; `Network::reset_after_poisoning`'s own unguarded
`socket_set.remove`, `litebox/src/net/mod.rs`; a `STATUS_STACK_OVERFLOW` in two more bare
`std::thread::spawn` sites needing the same 32 MiB `INITIAL_GUEST_THREAD_STACK_SIZE` treatment) but
**none were the `DE_FAILED` cause** — a live `execve` trace
(`litebox_shim_linux::syscalls::process=trace`) proved `xfce4-session`'s own envp holds correct
`DISPLAY=:1` bytes pre-loader, the ELF loader's stack-building code was inspected line by line and
found sound, and `/usr/bin/printenv` (a real `getenv()` call) reads `DISPLAY` correctly in the
identical invocation shape — narrowing the failure to something inside `xfce4-session`'s own
process.

**Passes 30-33 (2026-09-21/22), all FIXED/REFUTED, live-verified — full narrative:
`docs/AGENTS_ARCHIVE_2026-09-18.md`.** DISPLAY/`getenv()`/`environ` REFUTED FOR GOOD as the
`DE_FAILED` cause (LD_PRELOAD interposer, 19/19 real calls across a full boot returned the correct
`:1`, including inside GDK's own backend probe). AF_UNIX `connect_cross_process`'s
`EAGAIN`→`EINPROGRESS` errno-contract bug found+FIXED (mirrors the TCP path's existing override); a
second, related `SharedUnixConnectQueue` cancel-on-non-blocking gap found, scoped, left open
(`UnixStreamState::Connecting(request_idx)` needed). **Xvfb's own real crash root-caused**: a
SEPARATE, deterministic SIGSEGV ~190-207s into every full boot (not the `DE_FAILED` cause) — glibc's
AVX2 memcpy/memmove reading 64+ bytes from a wild, fully-unmapped pointer
(`0x7feffecdd400 == TASK_ADDR_MAX - 0x1312C00`, bit-identical `rip`/fault-address across boots);
`cdb` attach REFUTED as a capture method (perturbs the exact X11-traffic race the crash needs) —
use `LITEBOX_DIAG_FATALDUMP=1` (no debugger) instead; exact Xvfb call site emitting the bad pointer
still OPEN (needs real CFI-based unwinding or upstream Xvfb/glibc source cross-reference).
`ldconfig`/any static-PIE binary's double-relocation SIGSEGV root-caused+FIXED (`84a98bf`):
`ElfLoader::load` was still applying `R_X86_64_RELATIVE`/RELR relocations itself for a no-`PT_INTERP`
`ET_DYN` binary on top of static-PIE's own glibc/musl self-relocation, corrupting every RELR-covered
pointer to ~2x its value — fixed by never applying loader-side relocations to the main executable,
matching real kernel `binfmt_elf.c` behavior. `xfce4-session`'s own `Cannot open display: .` at
`DE_FAILED` remains OPEN, narrowed to "something inside `xfce4-session`'s own process" (envp/
`getenv()`/loader-stack all proven correct by direct evidence).

**Thirty-fourth pass (2026-09-22) — fork-eligibility mechanism confirmed by static code reading; no
by-name gate exists, the only gate is a global opt-in env var + a per-fork fd-kind scan; flipping
that env var on for the real desktop boot is confirmed still NOT a safe narrow win (a real, separate,
possibly-stale "Fork-after-Xorg" freeze risk, never re-tested since); live re-verification blocked
again by the SAME pre-Xvfb host-load condition the 33rd pass hit.**

**Thirty-fifth pass (2026-09-22) — the "Fork-after-Xorg PERMANENT freeze" risk is CONFIRMED GONE.**
Evidence: an already-on-disk real full boot (`.wfgy/webtop_release_boot5.log`, `LITEBOX_PROCESS_
FORK=1`, release binary, current code per `git log`/binary-mtime cross-check) shows the full `[s]`
marker sequence `XVFB_UP` → `DBUS_UP` → `SELKIES_LAUNCHED_LAST` → `DE_LAUNCHED` → `DE_FAILED` → the
script's own designed idle `HOLD` loop, with ZERO freeze/SIGSEGV/tcache-corruption anywhere — the
SAME final `DE_FAILED` state the thread-based path reaches, just via the cross-process path with no
crash class at all. **New bug found+FIXED from the same log**: the `SELKIES_PORT_UP` readiness
gate's 170-iteration `curl`-polling loop pays the FULL cross-process-child rootfs-rebuild cost per
`curl` fork (170x), almost certainly causing the log's own `SELKIES_PORT_SELFTEST_FAILED` and
`rc=137` OOM-kill — rewritten (`.wfgy/webtop_stack.sh`, gitignored/disk-only) to a zero-fork bash
`/dev/tcp/HOST/PORT` builtin check (same for the smaller 20-iteration `NGINX_SELFTEST` gate),
`bash -n`-clean, NOT yet live-verified. **Fresh live re-verification blocked this pass by the worst
host-RAM condition of the whole investigation** (0.4-1.3GB free throughout, confirmed unrelated to
litebox; a calibration re-run of the 09-17 pass's 24/24-clean minimal bare-fork repro died the same
non-diagnostic way the 34th pass saw at a BETTER 1.95GB — RAM remains the dominant confound, not a
regression). **`LITEBOX_PROCESS_FORK=1` is now the RECOMMENDED flag for `.wfgy/webtop_stack.sh`**
given the freeze evidence, superseding the 12th-34th-pass caution — evidenced-safe-pending-
reconfirmation, not fully closed until a fresh boot completes under workable RAM. **Pickup**: (1)
once RAM is genuinely quiet (~1.8GB+ sustained), re-run the full boot (debug binary first) to
confirm the fork-cost fix actually gets `SELKIES_PORT_UP` and no OOM-kill; (2) `DE_FAILED` is
unaffected and remains the sole real blocker on BOTH fork paths — still needs the live `cdb -pv`
attach on `xfce4-session` (`getenv`/`XOpenDisplay`/`_XConnectXCB`), not attempted by any pass to
date; treat the recurring `Cannot open display: .` text as this GTK/Xt build's generic
connection-failed fallback message, not literal evidence of `getenv("DISPLAY")`'s real return value
(that string recurs for totally unrelated root causes elsewhere in this project's own history).

**Thirty-sixth pass (2026-09-22) — `pty_registry`/`daemon_pty_masters` cross-process redesign
(pickup item 4b's pty slice): landed and compile/unit-verified; live cross-process byte-transfer
NOT yet confirmed this pass (confirmed the NEXT pass, 37th, see below).** Root cause matched the
SAME defect class already fixed nine times over
(`litebox`/`proc_self_info`/`pts_registry`/`elf_patch_cache`/`exec_ranges_cache`/
`segment_scan_cache`/`memfds`/`shared_files`/`unix_addr_table`/`fifo_registry`): both fields lived
directly on the byte-shared `GlobalState` struct as raw `BTreeMap<u32, PtyFd<Platform>>`s, whose
node pointers (and the `Arc<crate::channel::Channel>`/`Arc<Pollee>` they point at) are meaningless
outside the process that allocated them — a live crash waiting to happen the first time any
cross-process-forked child touched a pty, never yet hit only because no earlier pass's boot
happened to exercise it. UNLIKE `fifo_registry` (whose own doc comment explicitly disclaimed any
cross-process need), `pty_registry`'s own original doc comment claims real cross-process need
("any process that knows the id ... can open it", matching real devpts) — so a bare per-process
shadow (this fix's first half, `litebox_shim_linux/src/lib.rs`, eleventh instance of the pattern)
was not sufficient alone. Added `syscalls::pty::SharedPtyTable<Platform>` (new, `litebox_shim_linux/
src/syscalls/pty.rs`) as the genuine cross-process-visible half: an 8-slot fixed array (sized down
from an initial 32 after live-reproducing the SAME by-value-`GlobalState`-construction
`STATUS_STACK_OVERFLOW` `SHARED_UNIX_CONN_CAPACITY`'s own doc comment already documents — confirmed
this pass, via `cargo test -p litebox_shim_linux --lib syscalls::pty::`, that the identical failure
mode is PRE-EXISTING/environmental at the test harness's default thread stack size regardless of
this table, since it reproduces on unmodified `main` too; passes cleanly with `RUST_MIN_STACK` set
large, e.g. 64 MiB), each slot POD (termios/winsize/fg_pgid/locked/packet-mode, all `Copy`-safe) plus
two `syscalls::unix::SharedByteRing`s (reused directly, promoted `pub(crate)` in `unix.rs`, exact
same shape already proven sound for AF_UNIX) for the master<->slave byte data plane. Wired into
`ptmx_open`/`ptmx_closed`/`attach_pty_stdio` (publish/release alongside the local registry),
`pty_exists`/`live_pty_ids` (union of local + shared), and TWO new consumption paths: (a)
`GlobalStateHandle::pts_open`'s cross-process fallback -- constructs a fresh LOCAL
`PtyEnd::SharedSlave` fd-table entry when `id` is absent from this process's own registry but
present in `SharedPtyTable`; (b) `LinuxShim::pty_master_read`/`pty_master_write`'s fallback to
`SharedPtyTable::try_read_side`/`try_write_side` directly when `daemon_pty_masters` doesn't have
the id locally. A new `PtyStateRef` enum (originally `Local(&Arc<PtyPair>)` vs `Shared(id,
&SharedPtyTable)`, see the 37th-pass entry below for why the `Local` variant needed a second field)
lets `pty_ioctl` (`syscalls/file.rs`) and `PtyEnd::write`'s ONLCR-termios lookup work uniformly
across both transports with no duplicated logic. A new `poll_shared` helper (bounded 15ms re-poll,
mirroring `syscalls::unix::wait_on_events_polling`'s own loop shape) backs blocking reads/writes on
the shared path, since -- matching AF_UNIX's own already-documented finding -- no genuine
cross-process wakeup exists; a `PtyEnd::Shared{Master,Slave}` end's `IOPollable` impl conservatively
reports both `IN`/`OUT` always-possible rather than reaching for an `unsafe` 'static-erased table
reference or a second `FS` generic parameter on `PtyEnd`/`PtySubsystem`/`PtyFd` (both explicitly
out of scope this pass, documented in `pty.rs`'s own new "Shared cross-process pty data plane"
module doc comment alongside every other scope limit: no cooked-mode ONLCR/echo/DSR-reply
synthesis over the shared transport, no per-consumer close tracking on a `Shared*` end's own
`Drop`). Every ordinary LOCAL read/write additionally best-effort-mirrors its bytes into the
matching shared ring, so cross-process visibility works for a pty ANY process allocated, not only
ones a foreign process explicitly re-opens.

**36th-pass verification, precisely scoped**: (1) `cargo check -p litebox_runner_linux_on_windows_userland`
clean (real target, `backend_tracing` feature included, matching how this crate is actually built --
a bare `cargo check -p litebox_shim_linux` alone fails on an UNRELATED pre-existing issue,
`syscalls/file.rs`'s stderr-capture block hardcoding `litebox_util_log::__private::tracing`
without requesting the `backend_tracing` feature itself, confirmed pre-existing via `git stash`
against unmodified `main`, not touched this pass, out of scope). (2) All 14 pre-existing
`syscalls::pty::tests` cases pass unchanged (`RUST_MIN_STACK=67108864 cargo test -p
litebox_shim_linux --lib syscalls::pty:: --features litebox_util_log/backend_tracing`) -- proves
zero regression in the local, same-process transport (every echo/ONLCR/DSR/winsize/hangup/EPIPE
case). (3) A real guest boot (`debian:stable-slim`, debug binary, single host process, no
`LITEBOX_PROCESS_FORK`) running `/bin/sh -c 'script -qc "echo pty_ok" ...'` successfully allocated
a pty (`ptmx_open`/`TIOCGPTN`/`TIOCSPTLCK` all through the new `SharedPtyTable`-publishing path),
forked+`TIOCSCTTY`'d+`exec`'d its child (process tree showed `script`(pid2)/`sh`(pid3) both alive)
with no crash/panic/stack-corruption -- then stalled in `script`'s own interactive read-from-
real-stdin copy loop once given non-interactive/piped stdio (a harness-vs-tool mismatch consistent
with this project's own recurring "ash DSR-query hang"/"sys_ppoll stuck" theme, not a new
crash/panic; two attempts, one with piped empty stdin hit a real Unix `SIGPIPE`(13) on `script`'s
own output-copy write once its host-side stdout pipe context made that legitimate). **NOT
verified**: genuine cross-process pty I/O (`PtyEnd::SharedSlave`, `pts_open`'s shared fallback,
`pty_master_read`/`write`'s shared fallback) -- no `LITEBOX_PROCESS_FORK=1` boot was attempted this
pass.

**Thirty-seventh pass (2026-09-22) — genuine cross-process pty I/O over `SharedPtyTable` PROVEN LIVE,
plus two real, fundamental bugs found and fixed along the way (neither patched over).** Built a
tiny freestanding no-libc guest binary (`advisor/probes/pty_fork_probe.c`, host-cross-compiled:
`clang --target=x86_64-unknown-linux-gnu -nostdlib -nostdinc -ffreestanding -fno-stack-protector
-static -O1 -o pty_fork_probe pty_fork_probe.c`, same recipe as `bigfork_probe.c`/
`tramp_fork_probe.c`) that opens `/dev/ptmx`, unlocks + reads its id via `TIOCSPTLCK`/`TIOCGPTN`,
`fork()`s, then the PARENT writes a pid-tagged marker to the master strictly AFTER the `fork()`
call returns while the CHILD (no local `pty_registry` entry for an id it didn't allocate)
independently opens `/dev/pts/<id>` and polls it for the marker.

First attempt SIGSEGV'd immediately after `TIOCGPTN` with zero `[process_fork_diag]` output at
all -- root cause #1: `try_cross_process_fork` (`litebox_shim_linux/src/syscalls/process.rs`,
the fd-eligibility scan) had NO branch recognizing a pty fd as droppable-rather-than-refusing,
unlike close-on-exec/pipe/regular-file/eventfd fds -- so any process holding an open pty fd at
fork time (the master, in the ordinary `forkpty()`-shaped case this whole redesign exists for) had
its ENTIRE cross-process fork silently refused, falling back to the thread-based path and its own
known tcache-corruption class -- exactly what was observed. Fixed by adding a `None if
self.raw_fd_subsystem_name(*raw_fd) == "pty"` arm (new `dropped_pty` counter, parallel to
`dropped_cloexec`) that drops the fd and lets the fork proceed rather than refusing it: dropping is
correct because `SharedPtyTable::publish` already made the pty's existence/state/rings visible by
id to every process the moment `ptmx_open`/`attach_pty_stdio` created it, so the child re-opens
`/dev/pts/<id>` itself post-fork exactly as real devpts already requires "any process that knows
the id" to do.

Second attempt (after the fix above, plus fixing an unrelated codegen bug in the PROBE ITSELF --
see below) genuinely spawned a separate Windows process (`[process_fork_diag] task-resume-probe
(child, winpid=...)`) but the child's `open("/dev/pts/<id>")` failed `EIO` -- root cause #2:
`PtyStateRef::Local`'s mutating setters (`set_locked`, `set_termios`, `set_winsize`, `set_fg_pgid`,
`set_packet_mode`) only wrote to the LOCAL `Arc<PtyPair>`, never mirroring into the
`SharedPtyTable` slot the way `PtyEnd::write`'s byte path already does -- so a real
`TIOCSPTLCK(0)` unlock issued against the LOCAL master (the master is always local to its owning
process; only the slave is meant to be cross-process-opened) never reached the shared table's own
`locked` flag, leaving a cross-process `pts_open` permanently seeing the pty as locked (`EIO`)
regardless of what the owning process actually did. Fixed by widening `PtyStateRef::Local` to
carry `(&'a Arc<PtyPair<Platform>>, &'a SharedPtyTable<Platform>)` instead of just the `Arc`, and
having every `set_*` method write through to BOTH the local `Arc` and (best-effort, matching the
write-path's existing mirror contract) the shared slot.

Also found and fixed, in the PROBE ITSELF (not litebox): a `char buf[N] = "literal"` local array
initializer lowers, at `-O1`, to an aligned SSE `movaps` sized to the destination array -- this
freestanding, no-crt0 binary's `_start` -> `main` call chain does not reliably guarantee 16-byte
stack alignment at every call site the way real crt0 does, so this faulted with a genuine
`STATUS_ACCESS_VIOLATION` on the `movaps` itself (`[veh] RAWREGS ... rip=0x201743`, bytes `0f 29
84 24 d0 00 00 00` = `movaps [rsp+0xd0], xmm0`). Fixed with a manual byte-copy loop (`copy_str`)
instead of the array initializer -- recorded in `advisor/probes/README.md` as a lesson for any
future freestanding probe.

**Live evidence, the decisive run** (`.wfgy/pty_fork_probe_run8.log`, HOST env
`LITEBOX_PROCESS_FORK=1` set BEFORE invoking the runner -- NOT a guest `--env`, since
`spawn_cross_process_fork_child` in `litebox_platform_windows_userland/src/lib.rs` reads it via a
bare `std::env::var_os` on the HOST side; passing it as `--env` instead silently no-ops the whole
cross-process path with zero log output, a real early-session false negative this pass also hit
and diagnosed):

```
PROBE_START pid=1
PTMX_OPEN master_fd=3
TIOCSPTLCK rc=0
TIOCGPTN rc=0 id=0
PARENT_FORKED child_pid=2
PARENT_WROTE bytes=35 data=[MARKER_FROM_PARENT_PID_1_AFTER_FORK]
[process_fork_diag] task-resume-probe (child, winpid=26972): built Task, set fs_base=0x0, calling run_thread with
rip=0x2031e8 rsp=0x7fefffeefdb8 -- entering real guest execution
CHILD_START pid=26972 opening=/dev/pts/0
CHILD_SLAVE_OPEN slave_fd=3
CHILD_READ pid=26972 bytes=35 data=[MARKER_FROM_PARENT_PID_1_AFTER_FORK]
[process_fork_diag] task-resume-probe (child): run_thread returned (guest thread terminated)
[process_fork_diag] task-resume-probe (child): exiting with encoded status 0xc0de0000
PARENT_CHILD_EXIT status=0
PROBE_DONE
```

`winpid=26972` matches the child's own reported guest `pid=26972` (litebox assigns the real OS pid
to a genuine cross-process fork child), confirming this is a SEPARATE Windows process, not the
shim's own eligibility log alone (per this file's own standing "never trust the eligibility log
alone" lesson) -- and `CHILD_READ` shows the exact 35-byte marker the PARENT wrote to its LOCAL
master strictly AFTER `fork()` returned, read by the CHILD via a `SharedSlave` fd it opened fresh,
with no local `pty_registry` entry of its own. This is definitive, live proof that data written
after the fork point by one genuinely separate OS process crosses the `SharedByteRing` and is read
by a different genuinely separate OS process -- the actual cross-process goal `acb8615`'s own
commit message left unverified.

Verification: `cargo check -p litebox_runner_linux_on_windows_userland` clean; all 14 pre-existing
`syscalls::pty::tests` pass unchanged (`RUST_MIN_STACK=16777216 cargo test -p litebox_shim_linux
--lib syscalls::pty:: --features litebox_util_log/backend_tracing`); the run above, verified via a
real `--initial-files` tar boot of the freestanding probe under the DEBUG runner binary, exit code
0. Files: `advisor/probes/pty_fork_probe.c` (+ prebuilt `advisor/probes/pty_fork_probe`),
`litebox_shim_linux/src/syscalls/process.rs` (fd-eligibility `dropped_pty` arm),
`litebox_shim_linux/src/syscalls/pty.rs` (`PtyStateRef::Local`'s second field + mirrored setters).
