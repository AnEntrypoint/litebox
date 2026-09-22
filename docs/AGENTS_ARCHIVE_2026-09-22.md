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

## 38th pass (2026-09-22) — Xvfb SIGSEGV full symbolization narrative, verbatim pre-compaction text

DE_FAILED A/B-CONTROLLED: re-ran the isolation harness with ONLY the `xfce4-session` launch removed
(`.wfgy/de_noDE.sh`) — zero Xvfb crashes, well past the point the DE runs die, while still serving
`xset`/`xdpyinfo`/12 `xprop` polls. With the DE present the crash is 2/2 deterministic. Trigger is
`xfce4-session`'s SPECIFIC X11 traffic, not elapsed time, not X clients in general. `DE_FAILED` is
downstream of the Xvfb crash; one bug, not two.

Xvfb's own built-in `xorg_backtrace()` prints its frames to stderr on the fatal signal; always
emitted, always thrown away, because the harness sent Xvfb's stderr to `/tmp/xvfb.log` (unreadable
by the parent under `LITEBOX_PROCESS_FORK=1`) instead of a pipe. `... 2>&1 | sed 's/^/[xvfb] /' &`
fixed that. Captured frames, `.wfgy/de_only_1.log`: 13 frames, Xvfb text addresses in the
`0x158100xxxxx` band, glibc frames at `0x7fefedd7adf0` (signal frame) / `0x7fefede9dabd` (the
faulting AVX2 memcpy/memmove) / `0x7fefedd64ca8`+`0x7fefedd64d65` (`__libc_start_*`), fault address
`0x7feffecdd400` — bit-identical to the 32nd pass's register capture. `.wfgy/xvfb.debug` (BuildID
`6440f00c805782c9a39a5acd92855079e9fffc92`) has the DWARF.

Second independent capture (`.wfgy/de_only_2.log`) reproduced byte-identical module offsets and
fault address; only the load base moved (`0x15810000000` vs `0x7810000000`, both 256MB-aligned)
while every offset and the glibc addresses stayed fixed. Offsets: frame0 `0x1b20ed`, frame3
`0x74391`, frame4 `0x7530a`, frame5 `0x67f14`, frame6 `0xf631d`, frame7 `0x8254b`, frame8
`0x15444b`, frame9 `0x158514`, frame12 `0x447e1`.

Tested the constant-shift hypothesis against stock `.wfgy/xvfb.debug` across the whole admissible
delta window `0x4DBF..0x4DE1` (the range putting frame12 inside `_start`'s 34 bytes) — does not
resolve: frames 3-9 stay incoherent at every delta (`PanoramiXCopyPlane` + `PanoramiXPolyArc` +
`XineramaXvShmPutImage` + `input_option_set_value` + `ProcessVelocityData2D` is not a call chain,
and Xvfb does not run Panoramix at all). `frame12` mapping to `_start` is CIRCULAR (the delta was
solved to make it do so) and proves nothing by itself.

The rewriter-shifts-layout theory is REFUTED by bytes: the actual rewritten `/usr/bin/Xvfb` pulled
from `.litebox-cache` (4 copies, md5-identical, matching BuildID) has program headers and section
addresses byte-identical to stock (`.text 0x33900` len `0x166a45`, entry `0x3fa00`).
`litebox_syscall_rewriter` patches IN PLACE (5-byte `jmp` + `nop` padding into a trampoline region
mapped +16MiB away). Zero address movement, so both constant- and non-uniform-shift are dead.
Base-independent kill shot: `f12 - f9 = -0x113D33` while `_start - main = -0x10`. Sweeping all 1.2M
byte-aligned bases in the feasible range against the 20532 real return addresses: best agreement
4/9 at a non-page-aligned base, exactly chance (p≈0.0139/frame); no page-aligned base reaches 3/9.
At the true base 0/9 are return addresses and frame0 (`0x1b20ed`) is in `.eh_frame_hdr`, not
executable at all.

ROOT CAUSE, established this pass: libunwind's x86_64 `unw_is_signal_frame()` identifies the
sigreturn trampoline by matching the literal 9 bytes `48 c7 c0 0f 00 00 00 0f 05`
(`mov $0xf,%rax; syscall`) at the IP. In the rewritten libc, `__restore_rt` now reads `e9 40 42 1b
00` (`jmp`) + `90 90 90 90` — the rewriter replaced exactly those 9 bytes, and zero raw `syscall`
instructions remain anywhere in the rewritten libc. Signal-frame detection fails, the ucontext is
never read, and the walk desyncs into its RBP-chain fallback — every frame prints `?+0x0` and only
the ends (the `__restore_rt` word, the faulting IP, the `__libc_start_main` pair) are real.
Salvaged from the capture: `f2 = libc+0x162abd` = `vmovdqu (%rsi),%ymm0`, the 32-64-byte path of
`__memmove/__memcpy_avx_unaligned_erms`, with `%rsi = 0x7feffecdd400` — first instruction-granular
confirmation of the AVX2 claim. libc base `0x7fefedd3b000` in both runs (glibc Debian
2.41-12+deb13u3); the fault address sits ~20MB below `TASK_ADDR_MAX` (`0x7ff000000000`), well above
libc's top, i.e. in the high mmap/stack region, inside NEITHER module.

xfce4-session's real stderr had never been readable by any prior pass: the harness sent it to
`/tmp/de2.log`, unreadable by a parent under `LITEBOX_PROCESS_FORK=1`. Read for the first time via
a pipe, it contains NO "Cannot open display" at all — it contains an AT-SPI dbind-WARNING, which
GTK only emits AFTER `gtk_init` has already succeeded. The DE opens the display fine; Xvfb then
SIGSEGVs at the bit-identical `0x7feffecdd400` and the X server dies underneath it. Ordering in
`.wfgy/de_only_1.log` is unambiguous: `DE_LAUNCHED` :3818 → Xvfb SIGSEGV :5655 → DE AT-SPI warning
:5702 → `DE_FAILED` :10535.

New standing lessons, each live-proven this pass: guest diagnostics must travel by PIPE; `cmd >
/tmp/f` + parent read fails by SIZE, and `VAR=$(cmd)` returns EMPTY for an external command while
`$?` stays correct — root-caused to `litebox_shim_linux/src/syscalls/process.rs:2646-2649`'s
cross-process-fork carry-scan filtering `raw >= 3`, so guest fds 0/1/2 are never carried, silently
breaking any process that redirects stdout to a pipe before forking; fix shape recorded, not yet
implemented. `.wfgy/webtop_seed.tar` embeds a FROZEN copy of `webtop_stack.sh`; the 35th pass's
`/dev/tcp` gate rewrite had never executed in a guest because the tar predated it. A boot whose log
stops is usually a dead ROOT RUNNER, not a hang: children carry a bare 77-char command line, the
root carries the full `--oci-image` argument list — diagnose with one `Get-CimInstance` line rather
than assuming a stall.

## 39th pass (2026-09-22) — sigreturn-signature fix landed + verified; Xvfb call site narrowed by a direct raw-stack read

**Task 1, DONE and live-verified.** Implemented the fix the 38th pass's root-cause identified but
did not build: mirrored aarch64's existing `Task::ensure_sigreturn_trampoline` pattern onto x86_64
(`litebox_shim_linux/src/syscalls/signal/mod.rs`), `write_signal_frame`
(`signal/x86_64.rs`) now prefers it over the guest's `action.restorer`, and a new x86_64 branch in
`LinuxShimEntrypoints::exception` (`litebox_shim_linux/src/lib.rs`) catches the resulting
instruction-fetch fault and redirects into `sys_rt_sigreturn`. Full mechanism and rationale: this
commit's own message (`7d66935`) and the three files' doc comments. Key design point not obvious
from the diff: the trampoline page holds the REAL glibc `__restore_rt` bytes verbatim
(`48 c7 c0 0f 00 00 00 0f 05`), not a litebox-specific signature — `PROT_READ`-only (never
`PROT_EXEC`) is what makes catching execution safe without ever letting the real `syscall` opcode
decode, so the byte content stays meaningful to ANY non-CFI unwinder that pattern-matches it,
including glibc's own in-guest fallback and Xvfb's own `xorg_backtrace()`, not just this project's
tooling. `litebox_syscall_rewriter` itself is untouched — glibc's real `__restore_rt` still gets
patched like any other syscall site, but is now genuinely dead code (nothing returns there anymore
once a trampoline is allocated), so no `REWRITER_CACHE_VERSION` bump was needed.

Verification: `cargo build -p litebox_runner_linux_on_windows_userland` clean (debug profile).
Functional round-trip repro (`bash -c 'trap : USR1; kill -USR1 $$; echo AFTER_TRAP; echo DONE'`
under the debug runner, `.wfgy/sigreturn_repro1.log`) completed with `AFTER_TRAP`/`DONE` printed and
exit 0 — proves delivery → handler → new trampoline → instruction-fetch fault →
`sys_rt_sigreturn` → correct context restoration works end to end (a broken restore would have
hung, crashed, or skipped `AFTER_TRAP`). Re-ran `.wfgy/de_only.sh` (the existing Xvfb/xfce4-session
isolation harness) with the fix in place (`.wfgy/de_only_postfix_1.log`): the SAME bit-identical
SIGSEGV still reproduces (`addr=0x7feffecdd400 rip=0x7fefede9dabd`, libc base `0x7fefedd3b000`,
matching every prior capture exactly — confirms the fix does not touch the actual crash mechanism,
only backtrace fidelity), `DE_FAILED after 60s` still fires (Task 2 unaffected, expected). Xvfb's
own `xorg_backtrace()` output changed shape in exactly the direction predicted: instead of the old
13-frame walk (`0`, then `3`-`9`, then `12`) built by continuing past a corrupted signal frame into
stale stack words, it now prints only
```
0: /usr/bin/Xvfb (?+0x0) [0xc4101b20ed]
1: ? (?+0x0) [0x7feffff08000]
```
— frame0's offset (`0x1b20ed`) matches the OLD frame0 exactly (same fixed internal signal-handling
address, unaffected by the fix, as expected), and frame1 is a page-aligned address at the very top
of the guest address space, consistent with landing on the freshly `mmap`'d trampoline page rather
than continuing into libc/Xvfb .text — the walker no longer treats the signal boundary as ordinary
code and stops there instead of wandering into 11 more frames of stale RBP-chain garbage. This is
circumstantial (no direct log line prints the trampoline's own address to confirm the match
numerically) but consistent with the fix working as designed; a follow-up pass wanting certainty
should log the trampoline address at allocation time (`LITEBOX_LOG` trace on the signal module) and
diff it directly against this frame1 value.

**Task 2, narrowed but NOT fixed.** `LITEBOX_DIAG_FATALDUMP=1`'s raw `DIAG-STACKWALK` dump at the
moment of THIS pass's fault (`.wfgy/de_only_postfix_1.log:4155-4188`) gives a qword read directly
off the guest stack at `rsp=0x7fefffeed698` (the fault is inside `__memmove_avx_unaligned_erms`'s
hot AVX2 loop, a leaf routine with no extra stack push in the fast path, so `[rsp+0]` is the genuine
return address into the caller, not a frame-pointer guess): `0xc410074391`. With this run's Xvfb
load base `0xc4100000000`, that is offset `0x74391` — the SAME offset the 38th pass's `xorg_backtrace()`
walk called "frame3", now confirmed by an INDEPENDENT mechanism (a direct raw-stack read, not a
unwind walk that could itself be corrupted) to be the real, immediate caller of the crashing memmove,
not a stale/coincidental word. `addr2line -e .wfgy/xvfb.debug -f -C -i 0x74391` (DWARF-backed,
`.wfgy/xvfb_runtime.bin`'s bare `.symtab` disagreed and is less trustworthy — it lacks line-table
info entirely, confirmed by comparing the same query against both files) resolves this to
`ProcSELinuxGetClientContext`, `Xext/xselinux_ext.c:305` — the XSELinux extension's request handler
for the `GetClientContext` protocol request, i.e. Xvfb crashes while building/copying a reply to a
client that asked for its SELinux security context. The next four offsets up the OLD frame-pointer
walk (`0x7530a`→`SELinuxReceive` `xselinux_hooks.c:412`, `0x67f14`→`dixLookupPrivate`/
`CheckScreenPrivate` `saver.c:201`, `0xf631d`→`XkbCopyKeymap`, `0x8254b`→`glxProbeDriver`,
`0x15444b`→`acceleratePointerPredictable`, `0x158514`→`AddResource`, `0x447e1`→`fbBlt`) resolve to
mutually-unrelated Xorg subsystems (SELinux, Xkb, GLX, pointer acceleration, resource management, fb
blit) with no plausible real call relationship to each other or to frame3 — confirms the 38th pass's
own "probably not a true call chain" verdict for everything past the immediate caller: only the
literal top-of-stack word (frame3/`0x74391`) is trustworthy from this capture, matching this pass's
Task 1 fix landing correctly (the walker now correctly refuses to wander past the signal boundary
rather than fabricating 8 more frames of plausible-looking noise).

## Shared-memory foundations — full mechanism (drained from AGENTS.md's 39th-pass compaction, all DONE)

`RawMutex`: no longer calls `WaitOnAddress`/`WakeByAddressSingle` (process-local per MSDN) -- manual
wait queue + one auto-reset kernel `Event` per OS thread, cross-process half real
(`DuplicateHandle`-based); gained `poisoned: AtomicBool` + owner-death recovery;
`resolve_waiter_event`'s stale-pid panic fixed via a fixed-32-slot pointer-free `WaiterQueue`.
Shared kernel heap: a small 64 MiB, standalone, bounded `shared_kernel_arena_alloc` (`lib.rs`, NOT
wired to `GlobalAlloc`) backs `SharedArc<T>` (`value`+`strong: AtomicUsize`) for
`LiteBoxX`/`GlobalState` placement; `SLAB_ALLOC` stays on the private per-process path (a shared
bump allocator exhausted an 8 GiB pool in 45-90 execs).

Root cause of the whole `GlobalState`-sharing class, precisely characterized: `SharedArc::new`
shares only `T`'s literal inline bytes -- any REGISTRY that was a `BTreeMap`/similar has its NODES
on the private per-process heap, meaningless to an attaching process (PRD:
`globalstate-nested-collections-not-actually-shared`). Of the original list (`unix_addr_table`,
`pty_registry`, `daemon_pty_masters`, `flock_registry`, `fifo_registry`, `sysv_shm`, `memfds`,
`shared_files`): `unix_addr_table`/`fifo_registry`/`memfds`/`shared_files` are now
per-process-shadowed on `GlobalStateHandle`, `sysv_shm` is a real shared-arena-native fixed array.
`pty_registry`/`daemon_pty_masters` are ALSO now per-process-shadowed, with a genuine cross-process
companion restored separately via `syscalls::pty::SharedPtyTable` (a plain `GlobalState` field, same
pattern as `unix_addr_presence` below) rather than shadowed away, since real cross-process pty
visibility is actually needed (real devpts semantics: "any process that knows the id can open it")
-- this table is now live-verified genuinely cross-process (37th pass: a separate Windows process
opened a pty by id and read bytes a different Windows process wrote after `fork()`, over the table's
`SharedByteRing`s; two real mirroring bugs found+fixed to get there). `flock_registry` remains open
-- same non-POD-payload obstacle the pty fix's own pattern (fixed POD control state +
`SharedByteRing` data plane, no raw `Arc`/pointer ever crosses the process boundary) now gives a
concrete template for.

`SharedUnixAddrPresenceTable` (`syscalls/unix.rs`) established the reusable flat-table PATTERN
(`sysv_shm`, the AF_UNIX connection-DATA layer, and `SharedPtyTable` all reused it next): fixed-slot,
pure-atomic, lock-free `(kind, key bytes<=108, owner pid)` side-index, or a POD-control-state
variant for a table like `SharedPtyTable` that also needs mutable per-slot fields. `Pipes.litebox`'s
stale pointer (fixed via `GlobalStateHandle::pipes()`) and `FutexManager`'s stack-allocated
`LoanList` sharing hang (fixed: each process gets its own fresh `FutexManager`) were two more
instances of the same raw-pointer-frozen-into-shared-bytes root pattern. A mutable-state table
built on this pattern still needs every WRITE path individually audited for shared-side mirroring
-- `SharedPtyTable`'s own control-state setters (`set_locked` etc.) were originally wired ONLY into
the local `Arc`-backed side and silently never reached the shared slot until the 37th pass's live
cross-process test caught it; a table with read-only/publish-once shared state (like
`SharedUnixAddrPresenceTable`) doesn't have this failure mode, but any table modeling ongoing
mutable control state does. `SafeZoneAllocator::alloc`'s spinlock livelock (distinct mechanism, no
dead-holder recovery unlike `RawMutex`) is still open.

## Guest-diagnostic-must-travel-by-pipe — full mechanism (drained from AGENTS.md's 39th-pass compaction)

Three separate live-proven failure shapes, 38th pass, that invalidated a large amount of this
project's historical "we saw nothing, so nothing happened" reasoning:

`cmd > /tmp/f` then the parent reading `/tmp/f` — the forked CHILD writes into its own
writable-layer snapshot; the parent reads its own and sees nothing. Known gap, but its reach was
badly underestimated: it silently broke `xprop -root > /tmp/wm1; grep -q "window id" /tmp/wm1`,
i.e. the `DE_UP` check itself, so `DE_FAILED` could be reported no matter what the desktop did.
(Small files DO sometimes survive via `SharedFilePublishTable`'s 256-byte publish — `read -r A <
/tmp/addr` genuinely works for dbus's address — so this fails NON-deterministically by size, worse
than failing outright.)

`VAR=$(external-cmd)` returns EMPTY under `LITEBOX_PROCESS_FORK=1` while `$?` stays correct, and
the command's real output goes to the HOST CONSOLE instead. Root-caused to one line:
`litebox_shim_linux/src/syscalls/process.rs:2646-2649`'s cross-process-fork carry scan is
`raw_descriptors.iter_alive().filter(|&raw| raw >= 3)` — guest fds 0/1/2 are never carried across a
cross-process fork. The child instead rebuilds them unconditionally as fresh
`/dev/stdin|stdout|stderr` opens (`adopt_forked_process` → `initialize_stdio_in_shared_descriptors_table`,
`litebox_shim_linux/src/lib.rs:981`, body `:1410-1440`), which resolve to the CHILD process's own
`GetStdHandle` (`litebox_platform_windows_userland/src/lib.rs:9671-9689`). So any guest process
that redirected its own stdout onto a pipe BEFORE forking silently loses that redirection in the
child. The authorising comment at `process.rs:2630-2633` ("a cross-process child cannot carry
anything past the 0/1/2 stdio slots") is STALE — true only when fd 1 really is the host console.
Parent↔child pipe I/O itself is sound and was REFUTED as the cause (the bridge relays correctly;
`.wfgy/de_only_2.log` shows `total_relayed…=4/187/838` on the working shapes and `=0` ×13 on every
fork+exec case).

Why `$( )` fails but `|` works is fork DEPTH, not who holds the read end: `cmd | reader &` and
`$( builtin )` are ONE fork per stage, so `dup2(pipe,1)` happens after the fork inside the child's
own rebuilt fd table and sticks. `$( external-cmd )` is TWO forks — bash forks the comsub subshell,
that subshell does `dup2(3,1)`, then forks AGAIN to exec the binary — and at that second fork
stdout is already a pipe end sitting at fd 1, below the `raw >= 3` cut, so it's dropped. The
discriminator is "was stdout already redirected at fork time". `cmd 2>&1 | sed 's/^/[tag] /' &`
WORKS and is the shape proven to deliver a guest process's real output (how `xfce4-session`'s and
Xvfb's true stderr were read for the first time) — use it for every guest diagnostic.

FIX, scoped but NOT yet implemented (top of the pickup list): at `process.rs:2646-2649` scan ALL
alive fds, not `>= 3`; for each of 0/1/2 skip it only when it's still the plain `/dev/stdX` device
node (detectable via the `StdioStream` metadata set at `lib.rs:1435`) since host handle inheritance
already covers that, and otherwise classify it exactly like any other fd (pipe → `Sink`/`Source`
bridge, regular file → `ForkInheritedFile`, eventfd → `ForkInheritedEventfd`, else the existing
uncarriable/cloexec/pty rules). Delete the stale comment. The child side needs NO change — the
install loop runs after the stdio rebuild and `sys_dup`'s exact form (`file.rs:6330-6334`) already
displaces an occupied target. `spawn_exec_collision_child` (`platform lib.rs:12250-12255`) has the
SAME defect and the same signature, disclosed in its own comment. **Not attempted this (39th)
pass** — flagged in the task brief as needing its own dedicated, verified pass; still open.

Working hypothesis, NOT confirmed: `libselinux.so.1` is a real runtime dependency of this Xvfb build
(independently noted at `litebox_platform_windows_userland/src/lib.rs:8436`, an unrelated
already-fixed memory-claim bug that happened to be observed via this same library's load), so the
XSELinux extension is compiled in and its Proc handler is reachable regardless of whether real
kernel SELinux enforcement is active — a client (plausibly `xfce4-session` itself, or a library it
pulls in such as AT-SPI/dbus-glib, matching the A/B result that only the DE's own traffic triggers
this) can issue a `GetClientContext` request on any system. `litebox_shim_linux` has ZERO
SELinux-specific file/syscall emulation (`grep -i selinux` over the crate: no hits) — no
`/sys/fs/selinux`, no `security_getenforce` special-casing — so whatever Xvfb/libselinux do to
determine "am I enforcing" falls through to generic VFS emulation (almost certainly `ENOENT`,
correctly reporting SELinux as absent). Whether the crash is (a) a genuine upstream Xvfb/libselinux
defect on the "SELinux compiled in but disabled" fallback path for building a default/unconfined
context string reply, independent of litebox, or (b) litebox's generic VFS/getsockopt emulation
returning a malformed value some SELinux-adjacent lookup (e.g. a peer-credential check feeding
`XaceIsLocal`-style logic) doesn't validate before copying, was NOT distinguished this pass — source
for `xselinux_ext.c`/`xselinux_hooks.c` could not be fetched this pass (gitlab.freedesktop.org
blocked by Anubis anti-bot protection; the GitHub mirror path guessed was wrong and not
re-attempted). **Pickup, in order**: (1) fetch the real Xvfb source (try `apt source
xserver-xorg-core` inside a throwaway guest, or a correct GitHub mirror path/tag, rather than
gitlab.freedesktop.org) and read `ProcSELinuxGetClientContext` (`xselinux_ext.c:305`) plus whatever
it calls to build the reply, to determine the exact memcpy/length source; (2) if it traces back to a
litebox-emulated syscall, fix that syscall's return shape; (3) if it's a genuine upstream Xvfb
defect only reachable because SELinux is compiled in but inert, the cheapest real fix may be
disabling `XSELinux` at Xvfb's own build/init (`-extension SELinux` disable flag, if Xvfb's CLI
exposes one) rather than chasing a glibc/Xorg bug unrelated to litebox itself — verify this
disables the crash without disabling anything the desktop path actually needs first.

## 40th pass (2026-09-22) — `ProcSELinuxGetClientContext` lead REFUTED by direct experiment; real `statfs`/`fstatfs` correctness bug found+fixed

**Task: verify the 39th pass's `ProcSELinuxGetClientContext`/`xselinux_ext.c:305` hypothesis
against real source, root-cause litebox's side, fix it, and push to a full boot if fixed.**

**Real source obtained.** `gitlab.freedesktop.org/xorg/xserver` is still anti-bot-blocked, but
`github.com/XQuartz/xorg-server` (a real, actively-synced mirror — confirmed via
`api.github.com/repos/XQuartz/xorg-server/contents/Xext`, which lists `xselinux_ext.c`,
`xselinux_hooks.c`, `xselinux_label.c`, `xselinuxint.h` exactly as literal filenames) and
`github.com/SELinuxProject/selinux` (libselinux's actual upstream) both fetched cleanly via plain
`curl`. Read in full: `Xext/xselinux_ext.c`, `Xext/xselinux_hooks.c`, `libselinux/src/init.c`,
`libselinux/src/getpeercon.c`, `libselinux/src/enabled.c`.

**`ProcSELinuxGetClientContext` (xselinux_ext.c:290-305) does NOT read `/proc` or issue any raw
syscall of its own** — the 39th pass's own working hypothesis ("almost certainly reads
`/proc/<pid>/attr/current` or similar") was the wrong shape. The real function looks up an
ALREADY-CONNECTED X client by X resource ID (`dixLookupClient`) and replies with a SID cached on
that client's `devPrivates` at CONNECT time, via `SELinuxSendContextReply`. That caching happens in
`SELinuxLabelClient` (`xselinux_hooks.c:112-152`): it calls libselinux's `getpeercon_raw(fd, &ctx)`
and, on ANY failure, falls back to `SELinuxDefaultClientLabel()` — never a hard error on that path.
`getpeercon_raw` (libselinux `getpeercon.c`) is exactly `getsockopt(fd, SOL_SOCKET, SO_PEERSEC,
buf, &size)` on a `calloc`-zeroed buffer; on ANY negative return (any errno other than `ERANGE`,
which triggers one resize-and-retry) it frees the buffer and returns -1 with `*context` untouched —
the caller's `< 0` check then takes the fallback. **Checked litebox's actual `getsockopt`**:
`SocketOption` (`litebox_common_linux/src/lib.rs:2235-2251`) has no `SO_PEERSEC`(31) variant, so
`SocketOptionName::try_from(SOL_SOCKET, 31)` returns `None`, and `sys_getsockopt`
(`litebox_shim_linux/src/syscalls/net.rs:2569-2572`) answers with `ENOPROTOOPT` — exactly the shape
`getpeercon_raw` already handles via its fallback. **Not a bug.**

**The bigger finding: by this real source, the XSELinux protocol extension shouldn't even be
reachable in this guest.** `SELinuxExtensionInit` (`xselinux_ext.c:690-712`) calls
`AddExtension(SELINUX_EXTENSION_NAME, ...)` — the only place `ProcSELinuxDispatch` (and therefore
`ProcSELinuxGetClientContext`) becomes reachable at all — ONLY if `is_selinux_enabled()` returns
true first. `is_selinux_enabled()` (`libselinux/src/enabled.c`) is `return selinux_mnt &&
has_selinux_config;`. `selinux_mnt` is set by `init_lib()`'s constructor calling
`init_selinuxmnt()` -> `verify_selinuxmnt(SELINUXMNT)` (`SELINUXMNT` = `/sys/fs/selinux`), which
does `statfs(mnt, &sfbuf)` and only calls `set_selinuxmnt` if `sfbuf.f_type == SELINUX_MAGIC`
(`0xf97cff8c`). litebox's `statfs`/`fstatfs` handler unconditionally reports `f_type: TMPFS_MAGIC`
(`0x01021994`) — never `SELINUX_MAGIC` — so `selinux_mnt` should stay `NULL` and
`is_selinux_enabled()` should be false, regardless of whether the path even resolves. By this
analysis the extension registration, and therefore `ProcSELinuxGetClientContext`, should be
structurally unreachable.

**Decisive experiment, resolving the contradiction**: rather than trust either the 39th pass's
`addr2line` resolution or this pass's own source-reading over the other, ran the actual crash
repro with the `SELinux` extension EXPLICITLY disabled at Xvfb's own command line
(`-extension "SELinux"`, confirmed the correct protocol name via `#define SELINUX_EXTENSION_NAME
"SELinux"` in `xselinux.h`), added to a copy of `.wfgy/de_only.sh` seeded as
`.wfgy/de_only_noselinux_seed.tar`. Used `LITEBOX_PROCESS_FORK=1` (per the 35th pass, sidesteps the
thread-based-fork tcache-corruption class entirely) to get a clean, ~2-minute, reliable repro path
instead of fighting that unrelated flakiness. **Result, 2/2 runs**
(`.wfgy/de_only_noselinux_1.log:4205`, `.wfgy/de_only_noselinux_2.log`): Xvfb still crashes with the
SAME bit-identical `Segmentation fault at address 0x7feffecdd400`, comm=Xvfb, same shape as every
prior capture. **An explicitly-disabled X extension cannot be the crash site.** The 39th pass's
`addr2line -e .wfgy/xvfb.debug` resolution of the raw `[rsp+0]` stack word to
`ProcSELinuxGetClientContext`/`xselinux_ext.c:305` was therefore a misattribution — plausible
mechanism: the 39th pass reasoned this leaf AVX2 routine "pushes no extra stack" so `[rsp+0]` must
be the true return address, but that reasoning doesn't account for whatever happened in the fatal-
dump handler's own prologue between the fault and the read, or for stale/leftover stack content
from a prior call at that same address never having been overwritten. **Lesson recorded**: a single
raw-stack-word read is better evidence than a frame-pointer walk, but is NOT sufficient alone
without cross-checking against an independent mechanism (a live debugger attach, or a second read
from a genuinely different vantage point) — don't re-commit this same category of mistake next
pass.

**Independent bug found+FIXED while reading the `statfs` path for the above analysis**:
`SyscallRequest::Statfs { pathname: _, buf } | SyscallRequest::Fstatfs { fd: _, buf }`
(`litebox_shim_linux/src/lib.rs:2450`, pre-fix) discarded `pathname`/`fd` entirely and
unconditionally wrote a canned tmpfs-shaped `struct statfs` success for ANY path (even one that
doesn't exist) or ANY fd (even a closed/never-opened one). This is a real, independent correctness
violation of this project's own "guest-reachable code returns a real errno" rule, unrelated to
whether it explains the Xvfb crash (the magic-number analysis above shows it doesn't — `f_type`
stays `TMPFS_MAGIC` either way). **Fix** (same file): `Statfs` now resolves the path and calls
`self.sys_stat(path)?` before building the reply; `Fstatfs` calls `self.sys_fstat(fd)?` first; the
canned-reply construction itself was factored into a new free function `write_tmpfs_statfs` to
avoid duplicating the struct literal. Compiles clean (`cargo build -p
litebox_runner_linux_on_windows_userland`, debug profile, only pre-existing unrelated dead-code
warnings).

**Regression check, A/B, live-verified**: stashed the fix, rebuilt, reran the ORIGINAL
`.wfgy/de_only.sh` (no `LITEBOX_PROCESS_FORK`, matching the 39th pass's own shape) — hit the
pre-existing thread-based-fork tcache-corruption class immediately (every forked `bash` subshell
crashing with SIGSEGV/SIGABRT from ~13s in, `/tmp/.X11-unix/X1` never created, all X probes
`rc=139`; `.wfgy/de_only_baseline_ab_1.log`). Popped the stash, rebuilt, reran the identical
command (`.wfgy/de_only_statfsfix_1.log`) — **bit-identical failure, same line numbers, same
signal sequence** — proves this pre-existing flakiness (a SECOND, already-documented corruption
signature per AGENTS.md's own "still sporadically hits selkies" note) is a pure function of host
RAM/timing this session, NOT caused by the `statfs` fix. The fix is safe to keep. Under
`LITEBOX_PROCESS_FORK=1` (which sidesteps that whole class), the `statfs` fix in place, the Xvfb
crash still reproduces bit-identically (`.wfgy/de_only_statfsfix_pfork_1.log:4181`) — confirming,
as predicted by the magic-number analysis, that this fix alone does not touch the real bug.

**Net state at end of 40th pass**: the Xvfb SIGSEGV is UNCHANGED and still the sole real blocker;
the specific `ProcSELinuxGetClientContext` lead is closed for good (do not re-open without new
evidence of a different kind — see the lesson above); a real, verified, unrelated `statfs`/
`fstatfs` bug is fixed and committed. **Pickup, in order**: (1) a genuine live `cdb -pv` attach on
Xvfb itself, under `LITEBOX_PROCESS_FORK=1` for a fast/clean repro path (`.wfgy/de_only_seed.tar`,
~2 minutes to the crash, no `LITEBOX_DIAG_FATALDUMP` needed once a debugger has the process),
breaking on `__memmove_avx_unaligned_erms` or catching the AV directly to read the REAL register
state and call stack — not attempted by any pass to date, the single most promising next step; (2)
failing that, real CFI-based unwinding against `.wfgy/xvfb.debug`'s DWARF `.eh_frame` (a naive
raw-stack-word scan has now produced one CONFIRMED-WRONG lead in addition to the 32nd pass's
"4 plausible-but-probably-stale candidates" — stop trusting it alone); (3) full webtop boot /
browser-render verification remains blocked on (1)/(2) resolving first — not attempted this pass,
correctly, since the actual blocker is unchanged.

## 41st pass (2026-09-22) — live cdb attach finally attempted, verbatim pre-42nd-pass-compaction text

- **41st pass (2026-09-22) — live `cdb` attach finally attempted; the "cdb perturbs the race" belief
  is CONFIRMED, not refuted, by a clean 3-way A/B; pointer provenance narrowed with live register
  evidence; still no fix.** Rebuilt `target/debug` clean at current HEAD (`35f7c92`). **(1) Clean
  control, no debugger**: `.wfgy/xvfb_control42_boot.log`, `LITEBOX_PROCESS_FORK=1`,
  `LITEBOX_DIAG_FATALDUMP=1`, no cdb — crashed within ONE `WM_POLL` cycle (~10s after
  `xfce4-session` starts), bit-identical `0x7feffecdd400`, matching every prior pass. **(2) Live
  `cdb -p` attach with `sxe -c "gn" -c2 "r;k;.exr -1;u . L20;!analyze -v;qd" av`** (auto-continue
  every first-chance AV cheaply — required to ever intercept the real one, since litebox's own VEH
  returns `EXCEPTION_CONTINUE_SEARCH` on the unrecovered fault, which under a real debugger routes to
  a genuine second-chance event, confirmed by reading `litebox_platform_windows_userland/src/lib.rs`
  before attaching): attach succeeded cleanly (`.wfgy/xvfb_cdb41_22012.log`), boot proceeded through
  `DBUS_UP`→`DE_LAUNCHED_DIRECT`→all 12 `WM_POLL` iterations→`DE_FAILED after 60s`→600s `HOLD`
  loop with Xvfb alive and burning CPU throughout (208s→262s+) — **but NO second-chance AV ever
  fired in 10+ minutes**, vastly outside the crash's normal ~10-30s window; cdb itself burned
  500-600+ CPU-seconds intercepting the frequent benign first-chance AVs (litebox's own documented
  FS_BASE-reset recovery class), host RAM dipped to ~1.1GB free at one point (recovered). Killed and
  discarded (not one of this project's forbidden concurrent-full-verification runs — this is the
  lightweight `de_only.sh` isolation harness, and the two were never run overlapping). **(3) A
  single, UNCONDITIONAL one-shot `bp <fixed rip>` breakpoint** (no `sxe`, zero exception-interception
  overhead — `rip` is bit-identical across every capture, live and static, so a plain code breakpoint
  is valid): hit almost immediately, but on an unrelated, harmless call through the SAME
  `__memmove_avx_unaligned_erms` dispatch point (`rdx=0x224`, 548 bytes, read succeeded) — this libc
  entry point is shared by many ordinary memmove/memcpy call sites, not just the crashing one. The
  command auto-detached (`qd`) after this one hit, confirmed Xvfb still alive
  (`.wfgy/xvfb_bp44_13552.log`) — and **within the next 1-2 seconds, with NO debugger attached at
  all, the real crash fired** at the bit-identical address (`.wfgy/xvfb_bp44_boot.log:6484`). A
  same-PID reattach attempt 2 seconds later got `NTSTATUS 0xC000010A` ("attempt to access an exiting
  process") — Xvfb was already mid-death. **Verdict**: a bare, momentary attach+detach does NOT
  suppress the crash (it fired essentially the instant debugger overhead was removed) — it is
  specifically SUSTAINED interception/overhead (catching and handling every one of the frequent
  benign first-chance AVs) that reliably delays/prevents it, not "any debugger presence" per se. The
  32nd-pass caution was directionally right for the ONLY interception method every pass before this
  one could have used (blanket `sxe av`); it does not apply to a zero-overhead code breakpoint.
  **Concrete recipe for a future clean live capture, not yet executed**: a SINGLE persistent cdb
  session, attached once (no detach/reattach gap), with a CONDITIONAL breakpoint —
  `bp 0x7fefede9dabd ".if (@rdx = 0x40) {r;k;.exr -1;u . L20;!analyze -v;qd} .else {gc}"` (the
  `.if`/`.else` cdb block syntax, `gc` = go-from-conditional-breakpoint, cheaper than a full `g`) —
  filters out the many unrelated calls through this same dispatch point at near-zero cost and should
  finally catch the ACTUAL crashing call with live register/stack access, never needing `sxe` at all.
  This exact one-shot version (without the `.if` filter) was verified syntactically valid and DOES
  attach/set/hit correctly (see step 3 above) — only the conditional-filter variant remains
  untested end-to-end as one continuous session. **Pointer provenance, now with live register
  confirmation** (previously only inferred from static capture logs): `TASK_ADDR_MAX` for this
  platform is exactly `0x7FEFFFFF0000` (`HOST_ALLOCATOR_REGION_MIN - 0x10000`,
  `litebox_platform_windows_userland/src/lib.rs:7443,7454` — NOT the `lpMaximumApplicationAddress`
  the boot log's own "Max user address" line prints, which is a different, Windows-host-only value);
  fault address `0x7feffecdd400` = `TASK_ADDR_MAX - 0x1312C00` exactly (19,999,744 bytes / 19.07 MiB
  below the ceiling — arithmetic now verified, not just asserted). `rip=0x7fefede9dabd`,
  `rdx=0x40` (64 bytes) and the live disassembly (`vmovdqu ymm0,[rsi]` / `cmp rdx,40h` / `ja` / the
  paired tail-copy `vmovdqu ymm1,[rsi+rdx-20h]` — glibc's real `(32,64]`-byte memmove fast path) are
  bit-identical across every capture, this pass included. **New finding**: `rdi`/`rax` (destination,
  inside Xvfb's own non-PIE image) sit at a FIXED offset from Xvfb's own load base
  (`+0x1572d88`), but Xvfb's own base itself is NOT fixed across boots (`0xc4100000000` one run,
  `0x14c10000000` another, both this pass) — while `rsi`/the fault address stays bit-identical
  regardless, and glibc's own base is ALSO bit-identical across every boot
  (`0x7fefedd3b000`, ~271 MiB below the fault address — ruling out "just past glibc's own mapped
  image" as the source). The wild source pointer is therefore computed entirely independent of
  Xvfb's own (apparently non-deterministic) placement, and must derive from the DETERMINISTIC
  top-down shared-library-region layout alone. Windows' own view of the fault address:
  `type=0x0 protect=0x1 alloc_base=0x0` (`.wfgy/de_only_postfix_1.log:4146`,
  `.wfgy/xvfb_control42_boot.log`, both bit-identical) — genuinely FREE, unreserved memory, never
  mapped by litebox OR Windows. This is a DIFFERENT failure signature from the already-known,
  already-disabled "crowded top-down packing" bug (`litebox/src/mm/linux.rs:2439-2510`'s `if false`
  step 1.5, a WRITE into a PRESENT-but-wrong-permission page) — that disabled fix is real and
  plausibly related (same top-down-allocator root cause family, and its own doc comment cites
  measured gap sizes, 16.79-18.83 MB, in the same order of magnitude as this crash's 19.07 MB), but
  is NOT a confirmed match for THIS crash's exact mechanism (free vs. present-wrong-permission) and
  should not be blind-enabled without live confirmation. **The exact caller remains unresolved** —
  no capture this pass got a trustworthy resolved stack for the ACTUAL crashing call (the live `k`
  walk only ever captured the harmless, unrelated hit); the 40th pass's `ProcSELinuxGetClientContext`
  attribution stays REFUTED and closed. **Net state**: cdb-perturbation question answered for good
  (own root cause: sustained AV-interception overhead, not attachment itself); pointer provenance
  narrowed (independent of Xvfb's own base, tied to the deterministic library region, genuinely
  unmapped not mis-permissioned); no fix landed. **Pickup, in order**: (1) run the conditional-
  breakpoint recipe above as ONE continuous session (never detach before the real hit) to finally get
  a live, trustworthy register/stack capture of the ACTUAL crashing call, then resolve its caller
  against `.wfgy/xvfb.debug` and cross-check with a second independent capture before trusting it
  (this project's own standing lesson, twice-learned now); (2) once a real caller is known, decide
  whether the disabled `litebox/src/mm/linux.rs:2439-2510` step-1.5 top-down-packing fix is actually
  the same root cause (live-test by enabling it in isolation, on its own, exactly as its own comment
  already prescribes) or a separate-but-related defect needing its own fix.

## 42nd pass (2026-09-22) — executed the 41st pass's own conditional-breakpoint recipe; found and
## fixed FIVE independent, real bugs in it, none previously live-tested; final, precise conclusion:
## debugger-mediated capture of this crash appears fundamentally infeasible on this platform

Set out to execute the 41st pass's "concrete recipe, not yet executed":
`bp 0x7fefede9dabd ".if (@rdx = 0x40) {...} .else {gc}"` as one continuous cdb session. It had never
actually been run end-to-end. Five independent, real defects surfaced, each live-confirmed, each
fixed in turn, using `.wfgy/xvfb_pass42_orchestrator.ps1`/`xvfb_pass42_boot.ps1` (disk-only,
gitignored):

1. **Attaching immediately at the execve-detection instant races litebox's own guest-library
   mapping.** cdb's `bp` failed to insert (`Win32 error 299`, "Only part of a ReadProcessMemory or
   WriteProcessMemory request was completed") because the target page (inside glibc, mapped by
   litebox as part of handling this exact execve) was not yet committed in the host process; `g`
   then never actually resumed it, and the invasively-attached target sat suspended indefinitely
   (confirmed live via `Get-CimInstance Win32_Process`'s `UserModeTime`/`KernelModeTime` staying
   bit-identical across repeated checks — not merely slow, genuinely frozen). Fixed: a 2-second
   `Start-Sleep` between detecting the execve marker and spawning cdb (trivial margin; ELF/ld.so
   mapping finishes in low milliseconds of real time, the crash itself is a full ~10s+ away).
2. **Naive string-matching for the hit marker is a false-positive magnet.** The orchestrator's
   first version treated any occurrence of the literal text `REALCRASH_HIT` in the cdb output as a
   real hit — but cdb echoes its ENTIRE initial `-c` command verbatim right after attach (a
   `cdb: Reading initial command '...'` line), and that echo contains the literal `.echo
   REALCRASH_HIT` from the command text itself, matching even when the breakpoint had FAILED to
   insert at all (bug 1, above, was masked by this for one full attempt). Fixed: only trust a
   genuine `r`-command register dump (`rip=00007fef\`?ede9dabd` actually present in output).
3. **`rdx==0x40` alone is NOT a unique crash signature — live-disproven.** With bugs 1-2 fixed, a
   real, mechanically-correct hit fired: `rax=rcx=rdi=000003481133c860 rdx=0000000000000040
   rsi=00007fefffeebd00 rip=00007fefede9dabd`, disassembly `ds:00007fef\`ffeebd00=2d` — the read
   SUCCEEDED (a real byte value, not a fault), and `rsi` sat right next to `rsp`/`rbp`
   (`rsp=00007fefffeeb5c8`), an ordinary successful 64-byte stack-local copy during Xvfb's own early
   `xset`/`xdpyinfo` probe traffic, nowhere near `DE_LAUNCHED`. The 41st pass's own documented
   signature (`rdx==0x40`) was necessary but never actually sufficient — it was established from a
   SEPARATE, non-debugger register capture (`LITEBOX_DIAG_FATALDUMP`-class) at the moment of the
   real fault specifically, not shown to be exclusive to that fault. Fixed: condition on the exact
   WILD POINTER VALUE instead (`@rsi == 0x7feffecdd400`, bit-identical across 8+ prior captures per
   the 41st pass's own arithmetic), kept `rdx==0x40` as a cheap secondary check.
4. **cdb's default (MASM) expression evaluator does not support `&&`.** `.if (@rsi == 0x... &&
   @rdx == 0x40)` parsed the first `&` as bitwise-AND then errored on the second
   (`Numeric expression missing from '& @rdx == 0x40) ...'`), again leaving the session
   stopped/frozen (WMI-reconfirmed). Single `==` comparisons work correctly (proven twice, bugs 2
   and 3 above both used one). Fixed: nested `.if` blocks
   (`.if (@rsi == ADDR) {.if (@rdx == 0x40) {...} .else {gc}} .else {gc}`) instead of `&&`.
5. **`sxe` (enable) prints a banner line per intercepted event; `sxi` (ignore) does not, and this
   is NOT cosmetic.** Once Xvfb was doing real work (post-XSOCK), the FS_BASE-reset AV
   (`sxe -c "gn" av`, needed so litebox's own in-process VEH recovery still runs — see the 41st
   pass's own reasoning) fired thousands of times per second — the redirected cdb log grew
   ~86KB/s (~1900 "Access violation - code c0000005 (first chance)" lines/s). The PRINTING alone,
   not the `gn` continuation itself, reintroduces the exact "sustained interception overhead
   suppresses the crash" mechanism the 41st pass found for its heavier `-c2` diagnostic-dump
   variant — just via a different, previously-unconsidered cost source. Fixed: `sxi -c "gn" av`
   (silent — verified this keeps the SAME pass-through-to-app semantics, only suppresses the
   banner). A SIXTH, related defect: an unhandled first-chance "Single step exception" (code
   `80000004`) also stops the session by default and needed the identical `sxi -c "gn"` treatment
   (origin unconfirmed — possibly litebox's own occasional diagnostic single-stepping, unrelated to
   `fork_verify`'s THREAD-path-only healing since this is the cross-process-fork path; not
   root-caused this pass, just neutralized).

**With all six defects fixed, ran the corrected recipe to conclusion — full boot through
`DE_LAUNCHED_DIRECT`, all 12 `WM_POLL` cycles, `DE_FAILED after 60s`, into the designed `HOLD` loop.
Xvfb remained alive throughout the entire session; ZERO crashes, zero fault-address hits of any
kind.** This is the single most load-bearing new finding this pass: even after removing every
identified source of per-event overhead (no diagnostic dump on the vast majority of hits, no
per-event printing, a narrowly-scoped two-level conditional breakpoint, `sxi`'s silent pass-through
confirmed not to interfere with litebox's own FS_BASE recovery — the boot completed all its normal
milestones), the otherwise 100%-deterministic crash (8+ prior non-debugger captures, this
investigation's own control runs) did not occur even once. Real wall-clock elapsed ~15 minutes
(900s, the orchestrator's own deadline) for a boot that normally reaches the same `DE_FAILED` point
in roughly a minute undebugged — an order-of-magnitude slowdown despite every fix, meaning the raw
AV-interception RATE (thousands/second, confirmed by bug 5's log-growth measurement) still carries
real per-event scheduling/context-switch cost even with zero I/O and a near-instant `.if`
evaluation. **Conclusion, extending and sharpening the 41st pass's own finding**: this is not
merely "heavy diagnostic-dump overhead suppresses the crash" (the 41st pass's own framing, based on
its `-c2` variant) — even a session with NO per-event printing, NO per-event diagnostic dump, and a
target instruction hit rate low enough to keep cdb's own CPU near zero, still fully suppresses this
specific crash across a complete 60-second guest-relative window. The crash likely depends on
REAL WALL-CLOCK pacing of some underlying race (Windows scheduler quantum, a specific
inter-process timing window, or similar) that a debugger's `WaitForDebugEvent`-based interception
model cannot preserve regardless of how cheap each individual handler is — a stronger and more
precise statement than "sustained overhead", since overhead was minimized as far as this pass could
determine and the suppression was still total. **No live register/stack capture of the actual
crashing call was obtained this pass** — every real hit this pass produced was the same class of
false positive (bug 3), and the one clean full-overhead-fixed run never crashed at all. Top-down
crowded-packing cross-reference (`litebox/src/mm/linux.rs:2439-2510`'s disabled step 1.5): NOT
newly confirmed or refuted this pass — no real crash was captured to compare against, so its own
prescribed isolated-enable test remains the correct next step, unchanged from the 41st pass's own
pickup item. **Pickup, in strict order for the next pass**: (1) do NOT re-attempt live cdb capture
of this crash without a genuinely different interception mechanism — a user-mode debugger's
exception-routing model is now evidenced, not just suspected, to be incompatible with reproducing
it, regardless of per-event cost; (2) try a kernel ETW trace instead (no `WaitForDebugEvent`
round-trip per event, may avoid the same class of perturbation) if a capture is still wanted; (3)
more promising and untried: add a permanent, cheap, allocation-free DIAGNOSTIC LOG inside litebox's
own translated-code memmove/memcpy dispatch (or its top-down `Vmem` placement logic directly) that
records the source pointer of every copy above some threshold size to a small ring buffer BEFORE
the crash can happen, so the wild pointer's origin becomes visible from an ordinary
`LITEBOX_DIAG_FATALDUMP=1` capture (already proven non-perturbing, 32nd pass) with no live debugger
involved at all; (4) alternatively, live-test the disabled `linux.rs:2439-2510` step-1.5 fix in
isolation exactly as its own comment prescribes (still not done by any pass) — even without a
resolved caller, if enabling it makes the crash stop reproducing across N clean non-debugger runs,
that is itself strong evidence for (not proof of) the same-root-cause theory the 41st pass raised.
Full evidence, exact orchestrator script diffs, and every intermediate (partial) capture log:
`.wfgy/xvfb_pass42_*` (gitignored, disk-only as of this writing).

## 38th-40th passes — verbatim pre-42nd-pass-compaction "CURRENT STATE" narrative

- **`DE_FAILED` IS THE Xvfb SIGSEGV, not a second bug — 38th pass, direct evidence.**
  `xfce4-session`'s real stderr (never readable by any prior pass — see pipe lesson above)
  contains NO "Cannot open display"; it contains an AT-SPI dbind-WARNING GTK only emits AFTER
  `gtk_init` succeeds. Log ordering: `DE_LAUNCHED` → Xvfb `Segmentation fault at
  0x7feffecdd400` → the DE's AT-SPI warning → `DE_FAILED`. A/B-CONTROLLED: the same harness with
  only `xfce4-session`'s launch removed (`.wfgy/de_noDE.sh`) runs zero Xvfb crashes. Do NOT reopen
  DISPLAY/`getenv()`/loader-stack (proven correct, 30th pass) or attach `cdb` to `xfce4-session` —
  it is not the faulting process.
- **Xvfb's crash backtrace was corrupted PROJECT-WIDE, ROOT-CAUSED (38th pass) and FIXED (39th
  pass, `7d66935`).** Cause: libunwind's x86_64 signal-frame detection matches the literal 9 bytes
  `48 c7 c0 0f 00 00 00 0f 05` (`mov $0xf,%rax; syscall`) at `__restore_rt`, and
  `litebox_syscall_rewriter` overwrites exactly those bytes in place to intercept every guest
  `syscall` (no seccomp/ptrace on this platform) — desyncing every guest backtrace through any
  delivered signal, not just Xvfb's. Fix: x86_64 signal delivery now returns through a
  litebox-synthesized trampoline (mirroring aarch64's existing `ensure_sigreturn_trampoline`)
  instead of `action.restorer` — a freshly `mmap`'d, `PROT_READ`-only page holding the REAL glibc
  bytes verbatim (so any non-CFI unwinder, including Xvfb's own `xorg_backtrace()`, still
  recognizes it), while reaching it via `ret` faults on instruction-fetch before the real
  `syscall` opcode decodes (caught in `LinuxShimEntrypoints::exception`,
  `litebox_shim_linux/src/lib.rs`). Verified: signal round-trip works end to end; Xvfb's own
  backtrace post-fix now correctly stops at the signal boundary instead of wandering 11 frames
  into stale RBP-chain garbage. Full narrative: commit `7d66935`.
- **The Xvfb SIGSEGV itself remains OPEN — the 39th pass's `ProcSELinuxGetClientContext` theory is
  REFUTED, 40th pass, direct empirical evidence, real upstream source now in hand.** Fault is
  `libc+0x162abd` = `vmovdqu (%rsi),%ymm0` inside `__memmove_avx_unaligned_erms`, reading a wild,
  fully-unmapped, bit-identical `0x7feffecdd400` across every capture (now 8+ captures, 40th pass
  included). Real `Xext/xselinux_ext.c`/`xselinux_hooks.c`/libselinux `init.c`/`getpeercon.c`
  fetched this pass (`github.com/XQuartz/xorg-server`, `github.com/SELinuxProject/selinux` —
  gitlab.freedesktop.org is still anti-bot-blocked but this mirror isn't) and read in full:
  `ProcSELinuxGetClientContext` (`xselinux_ext.c:290-305`) does NOT touch `/proc` at all — it looks
  up an already-connected X client by resource ID (`dixLookupClient`) and replies with a SID set at
  CONNECT time by `SELinuxLabelClient` (`xselinux_hooks.c:112-152`), which calls libselinux's
  `getpeercon_raw(fd, &ctx)` (`getsockopt(SOL_SOCKET, SO_PEERSEC)`) and falls back to
  `SELinuxDefaultClientLabel()` on ANY failure — confirmed this pass that litebox's `getsockopt`
  already answers unmapped options (no `SO_PEERSEC`/31 variant exists in `SocketOption`,
  `litebox_common_linux/src/lib.rs:2235-2251`) with `ENOPROTOOPT`
  (`litebox_shim_linux/src/syscalls/net.rs:2569-2572`), which is exactly the failure shape
  `getpeercon_raw` already handles by falling back — NOT a bug. More importantly: the XSELinux
  protocol handler is only ever registered via `AddExtension` inside `SELinuxExtensionInit`
  (`xselinux_ext.c:690-712`) if `is_selinux_enabled()` returns true, which itself requires
  `statfs("/sys/fs/selinux", …).f_type == SELINUX_MAGIC` (`libselinux/src/init.c`
  `verify_selinuxmnt`) — a check litebox's `statfs` could never satisfy (see the fix below), so by
  the real source the extension should never even init. **Decisive test, 40th pass**: reran
  `.wfgy/de_only.sh` under `LITEBOX_PROCESS_FORK=1` (sidesteps the thread-fork tcache-corruption
  class entirely, giving a clean, fast, reproducible path to the crash — 2/2 runs,
  `.wfgy/de_only_statfsfix_pfork_1.log`) with Xvfb launched with `-extension "SELinux"` added
  (explicitly disabling the extension at the protocol level, `.wfgy/de_only_noselinux_seed.tar`):
  **the SAME bit-identical `0x7feffecdd400` SIGSEGV still occurs, 2/2 runs**
  (`.wfgy/de_only_noselinux_1.log`, `.wfgy/de_only_noselinux_2.log`). An explicitly-disabled
  extension cannot be the crash site — the 39th pass's `addr2line`-based attribution to
  `ProcSELinuxGetClientContext`/`xselinux_ext.c:305` was a misattribution (the `[rsp+0]`
  raw-stack-word read, though a sounder method than a frame-pointer walk, was not actually the true
  return address this time — do not re-trust a single raw-stack-word capture without an independent
  cross-check again). **Do not re-open the SELinux/XSELinux/`is_selinux_enabled`/`getpeercon`
  thread — fully closed, 40th pass.** The real crash site is UNKNOWN again; next step needs a
  genuine live debugger attach on Xvfb itself (`cdb -pv`, breaking on `__memmove_avx_unaligned_erms`
  or its caller) or CFI-based unwinding, neither attempted by any pass to date — a raw-stack-word
  scan alone has now produced one confirmed-wrong lead and should not be trusted alone again.
  **Independent, unrelated bug found+FIXED same pass**: `SyscallRequest::Statfs`/`Fstatfs`
  (`litebox_shim_linux/src/lib.rs:2450` pre-fix) ignored `pathname`/`fd` entirely
  (`pathname: _`/`fd: _`) and unconditionally returned a canned tmpfs-shaped success for ANY path
  or fd number, including nonexistent/closed ones — a real, independent correctness bug (statfs on
  a nonexistent path, or fstatfs on a bad fd, must fail) now fixed by validating via `sys_stat`/
  `sys_fstat` first (a real function, `write_tmpfs_statfs`, factored out and reused by both);
  live-verified via an A/B stash/rebuild/rerun that it does NOT regress the pre-existing
  thread-fork tcache-corruption flakiness (bit-identical failure with and without the fix) and does
  NOT (as hoped) eliminate the Xvfb SIGSEGV either, consistent with the magic-number analysis above
  (litebox's `f_type` is `TMPFS_MAGIC`, never `SELINUX_MAGIC`, before or after this fix).

## 43rd pass (2026-09-22) -- the Xvfb SIGSEGV is FIXED: enabled the disabled top-down-packing
## walk-down fix (linux.rs:2439-2510 step 1.5), live A/B-confirmed same-session, control crashes
## every time, fix survives every run including two full webtop_stack.sh boots

Per the 42nd pass's own pickup item (4) (live cdb capture proven fundamentally infeasible; the next
step was to isolated-enable the disabled get_unmmaped_area step 1.5 and compare boot outcomes,
never a live debugger). Read linux.rs:2439-2510 and its own commit (75eb781, 6th pass, "mm: never
place two searched mappings flush against each other") in full first: that commit landed TWO
fixes -- the inter-mapping MAPPING_GUARD_GAP (already enabled, fixes a DIFFERENT, older "forked
Xorg"/"Xorg as pid 1" WRITE-into-neighbour corruption class, both already closed) -- and a SECOND,
separate fix (step 1.5) left `if false`d on purpose: when the top-down fast path's single candidate
(high_limit) is foreclosed because the topmost existing VMA already reaches past it, step 1.5 walks
DOWN from high_limit past the mappings occupying that window and takes the first gap that fits,
instead of giving up on the whole upper region and falling through to step 2's strictly-below-
existing-mappings search -- which packs an execve'd process's libraries into a crowded low window
with inter-library gaps as small as one page. The commit's own doc comment already named the
population this governs (DIAG_UNMAPPED events sized 16.79-18.83MB, matching real ELF-segment-plus-
DEFAULT_RESERVED_SPACE_SIZE reservations) and explicitly deferred it because it addresses "a REAL,
separately-measured defect, but not the [WRITE-into-neighbour] one" -- never live-tested against
the Xvfb SIGSEGV (a READ from genuinely-unmapped memory, a different fault mechanism) by any pass
until this one, despite the 41st pass's own pointer-provenance finding (fault address independent
of Xvfb's own ASLR base, tied to the deterministic top-down library region, same order of magnitude
as step 1.5's own measured gap sizes) making the connection plausible.

Hypothesis: if crowded top-down packing is what produces this specific 19.07MB-below-ceiling wild
pointer (not just the older WRITE-into-neighbour class), enabling step 1.5 should stop the crash
even though nobody has resolved the actual crashing call site.

Method (non-debugger, per the 42nd pass's own conclusion): same-session A/B using the isolated
de_only.sh harness (.wfgy/de_only.sh, Xvfb -> dbus -> xfce4-session, no nginx/selkies) plus a real
webtop_stack.sh full-stack boot, both under LITEBOX_PROCESS_FORK=1, comparing DEBUG-BUILD boot
outcomes with step 1.5 `if true` vs `if false`, never attaching a debugger.

Results, all same session, same host, same .wfgy/de_only_seed.tar:

- Control (step 1.5 `if false`d -- every prior pass's actual baseline, rebuilt fresh this pass to
  rule out any other confound): .wfgy/xvfb_pass43_control2.log -- Xvfb segfaults at the
  bit-identical wild pointer 0x7feffecdd400 within ONE WM_POLL cycle ("(EE) Segmentation fault at
  address 0x7feffecdd400" / "(EE) Caught signal 11"), matching all 8+ non-debugger captures across
  the 38th-42nd passes exactly. (A first control attempt, .wfgy/xvfb_pass43_control.log, hit the
  ALREADY-DOCUMENTED, unrelated SafeZoneAllocator spinlock livelock -- Track B pickup #3 -- spinning
  forever pre-XSOCK; it resisted Stop-Process -Force exactly as AGENTS.md's own standing lesson
  predicts and needed Invoke-CimMethod -MethodName Terminate; retried clean.)
- Fixed (step 1.5 enabled): TWO independent de_only.sh runs, same conditions -- run 1
  (.wfgy/xvfb_pass43_run1.log) went through the FULL 60s WM_POLL window to DE_FAILED after 60s with
  ZERO Segmentation fault/SIGSEGV anywhere in the log; run 2 (.wfgy/xvfb_pass43_run2.log)
  independently confirmed clean through the same crash window a second time (stopped early once
  past it, by design, to conserve RAM for the next test).
- Fixed, full stack: TWO independent webtop_stack.sh release-binary boots
  (.wfgy/webtop_pass43_boot1.log, .wfgy/webtop_pass43_boot2.log, LITEBOX_PROCESS_FORK=1 as a HOST
  env var, --env GLIBC_TUNABLES=... still passed defensively though moot on this path) both reached
  NGINX_STARTED -> XVFB_UP -> DBUS_UP -> SELKIES_LAUNCHED_LAST -> SELKIES_PORT_UP -> DE_LAUNCHED ->
  DE_FALLBACK_LAUNCHED -> DE_FAILED with `grep -i "segmentation\|sigsegv"` returning ZERO matches in
  either full log. This is the first time in the whole 6th-43rd-pass investigation the full real
  webtop boot has run Xvfb-crash-free end to end.

Fix committed: 01f8532 -- `if false && last_end > high_limit` -> `if last_end > high_limit`,
the clippy overly_complex_bool_expr expect removed (no longer dead), full A/B evidence in the
commit message.

DE_FAILED itself is UNCHANGED and remains open -- both full-stack boots reached it via the SAME
WM2_PROBE-never-sees-_NET_SUPPORTING_WM_CHECK path documented since the 38th pass; this was never
in this pass's scope (the 30th-pass DISPLAY/getenv()/loader-stack proof and the narrowing to
"something inside xfce4-session's own process" both stand unchanged) and is now the SOLE remaining
blocker to a fully interactive browser desktop, cleanly separated from the crash for the first time
(previously the crash made it impossible to tell how much of DE_FAILED was Xvfb-crash noise vs a
genuine second defect -- the 38th pass's own A/B already answered that correctly: it's one bug seen
from two ends, and fixing the crash does not, and was never expected to, also fix DE_FAILED).

New, unscoped observation, not chased further this pass: host-side `curl http://localhost:8081/`
returned connection-refused/empty-reply on both full-stack boots despite the GUEST's OWN
/dev/tcp/127.0.0.1/8081 self-test succeeding (SELKIES_PORT_UP curl_exit=0) -- a possible --publish
port-forward gap, or simply RAM-contention fallout (host free RAM fell to 0.4-0.6GB at almost
exactly the DE_LAUNCHED/DE_FALLBACK_LAUNCHED point in BOTH boots, recovering afterward each time --
the same transient dip the 35th pass documented at the same script stage, not new). Not
investigated further per this pass's own scope (the Xvfb crash) and per the standing "don't
rabbit-hole on browser connectivity" guidance; real claude-in-chrome/chrome-devtools MCP screenshot
verification was not attempted this pass (moot without host-side port reachability, and moot
regardless without DE_FAILED resolved -- no window manager means nothing to screenshot).

Verification discipline notes: two runs alone would not have been sufficient confidence per this
project's own repeated "single clean run proves nothing" lesson -- the control-crashes-every-time
result in the SAME session, on the SAME seed tar, with ONLY the one line changed, is what makes
this a real A/B rather than two independent coincidences (host RAM/load conditions differed between
passes historically; holding everything else fixed within one session removes that confound).
LITEBOX_DIAG_FATALDUMP=1 and LITEBOX_DIAG_FORK_TIMING=1 were NOT needed for any of this -- plain
boot-outcome comparison via [s]-tagged markers and a `grep -i segmentation` sufficed, exactly the
"non-perturbing, non-debugger" method the 42nd pass's own pickup list prescribed.

Pickup for the next pass: (1) DE_FAILED -- the sole remaining blocker, unchanged scope from every
pass since the 30th (needs a live cdb -pv attach on xfce4-session itself, breaking on
getenv/XOpenDisplay/_XConnectXCB -- never attempted by any pass to date, and now finally SAFE to
attempt without the Xvfb-crash confound in the way); (2) the host-side port-8081 unreachability
observation above -- reproduce once RAM is holding above ~2GB throughout, to tell apart a real
--publish gap from the already-documented transient dip; (3) once DE_FAILED is closed, real
browser/app verification (Terminal Emulator, Thunar per SharedPtyTable) becomes reachable for the
first time in this investigation's history.

## 44th pass full detail (drained from AGENTS.md by the 45th pass's own compaction)

- **44th pass (2026-09-22) — two real bugs FIXED+verified; `DE_FAILED`'s previously-understood
  cause is closed, but a SECOND, deeper blocker (a related/same-class Xvfb crash) was newly exposed
  and is now open.** (1) `try_cross_process_fork`'s fd-eligibility scan (`process.rs:2646-2666`)
  hard-cut at `raw >= 3`, so a redirected 0/1/2 (`cmd 2>&1 | sed &`'s SECOND fork, or ANY
  `$(external-cmd)` two-fork comsub) silently lost its real pipe and got a fresh, wrong
  `CreateProcessW`-inherited stdio handle instead — root cause of "guest diagnostics must travel by
  pipe" (38th-39th passes) NEVER actually being fixed by switching to `$( )`/pipes, since the SAME
  gap ate the redirected fd at the fork boundary regardless. Fixed: fds 0/1/2 are now scanned like
  any other fd unless `raw_fd_is_plain_stdio_device` (new, `file.rs`, keyed on `StdioStream`
  metadata surviving a `dup2`) says they're still the untouched device node. Live-verified: a full
  `webtop_stack.sh` release-binary boot (`.wfgy/webtop_pass44_boot1.log`) now shows genuinely
  non-empty `WM1_PROBE`/`WM2_PROBE`/`PRE_DE_XDPYINFO` content (`_NET_SUPPORTING_WM_CHECK: not
  found.`, real `xdpyinfo` output) for the first time ever — every prior pass's `$( )` captures of
  these were silently empty regardless of the real answer. (2) `connect_cross_process`'s
  non-blocking-miss arm (`unix.rs`, ~1594) unconditionally `cancel()`led the just-posted
  `SharedUnixConnectQueue` request before returning `EINPROGRESS` — so ANY correct non-blocking
  AF_UNIX client (poll for writable, don't retry `connect()`) could poll forever on a request this
  shim had already withdrawn. Live-caught as the loop `xfce4-session` sits in forever: a debug trace
  (`litebox_shim_linux::syscalls::unix=debug`) shows `self_pid` matching xfce4-session's own guest
  pid hit exactly this arm, and `tid=27`/`tid=21572` (its own guest tid across two runs) NEVER calls
  `clone()`/`fork()` again afterward -- zero session-client children, not even `xfwm4`. Fixed: new
  `UnixStreamState::Connecting(UnixConnectingStream)` keeps the request alive; `check_io_events`
  (now mutating, `with_state` not `with_state_ref`) re-checks `poll_result` on every poll/epoll tick
  and completes the connection in place; a repeated `connect()` on the same fd also re-checks
  (`EALREADY` while pending, completes if ready) rather than re-posting. Live-verified via
  `.wfgy/de_only_pass44_run2.log`: `xfce4-session` now genuinely progresses for the first time ever
  past its D-Bus setup -- `iceauth`, `ssh-agent`, `gpg-agent`, `xfconfd`,
  `dbus-update-activation-environment` all `execve` for the first time in this whole investigation's
  history (none appear in ANY prior pass's log). **`DE_FAILED` is STILL OPEN**: before `xfwm4` is
  ever reached, Xvfb SIGSEGVs again -- `.wfgy/de_only_pass44_run2.log:46383-46392`, fault address
  `0x4000400` (NOT the 43rd pass's `0x7feffecdd400`), but backtrace frame0's offset is
  BIT-IDENTICAL (`0x1b20ed`, still inside `.eh_frame_hdr`, still the same broken-unwind signature)
  -- strong evidence this is the SAME underlying wild-pointer-read defect the 32nd-43rd passes
  chased, just reached via a trigger condition the 43rd pass's crowded-top-down-packing fix (step
  1.5) does not cover -- plausibly because `xfce4-session`'s now-much-deeper startup (five more
  real binaries `dlopen`-ing their own library trees) produces a differently-crowded top-down
  window than the fix's own validated repro did. Also real but likely NOT fatal on its own (GLib
  `CRITICAL` doesn't `abort()` by default): `xfce4-session`'s real stderr, readable for the first
  time via the now-working pipe fix, shows `libxfce4util-WARNING: Failed to get a ConsoleKit proxy:
  Could not connect: Connection refused` (expected -- no system bus is started, matching real-world
  bare-Docker XFCE reports) immediately followed by a burst of `GLib-GObject-CRITICAL: invalid
  (NULL) pointer instance` / `g_signal_connect_data` / `g_dbus_proxy_call_sync_internal` assertion
  failures -- xfce4-session's own ConsoleKit-absent code path not null-checking before use; almost
  certainly cosmetic noise real XFCE-in-Docker deployments already tolerate, not chased further that
  pass.

## 45th pass full evidence (log paths, live commands)

Pre-fix baseline: `.wfgy/de_only_pass44_run1.log` (clean, 0 host-panics, 0 Xvfb SIGSEGV),
`.wfgy/de_only_pass44_run2.log` (6 host-panics at `lib.rs:7396` between lines 35886-45659, PLUS a
genuine Xvfb SIGSEGV at line ~46383, fault address `0x4000400`, backtrace `Xvfb (?+0x0)
[0x4101b20ed]`). Post-fix (both fixes, `5d63ec6`+`32dd3d5`): `.wfgy/de_only_pass45_run1.log` (0
host-panics), `.wfgy/de_only_pass45_run2.log` (3 host-panics, same address), `.wfgy/
de_only_pass45b_run1.log` (5 host-panics, same address, one during `ssh-agent`'s own exit at line
41331) -- zero Xvfb SIGSEGV in any of the three post-fix runs, but sample size (3) is too small to
claim that class is fixed, especially since the host-panic bug still firing on every post-fix run
changes downstream process timing/ordering (a possible confound for a crash this project has
previously shown to be timing-sensitive). Repro command (isolation harness, ~100-500s per run,
much cheaper than the full `webtop_stack.sh`):

```
$env:LITEBOX_PROCESS_FORK = "1"
& .\target\release\litebox_runner_linux_on_windows_userland.exe --env GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0 --oci-image docker.io/linuxserver/webtop:debian-xfce --resume-from .wfgy/de_only_seed.tar -- /bin/bash /de_only.sh *> out.log
```

A `LITEBOX_DIAG_PROCESS_FORK_EXEC_FIXUP=1`/`LITEBOX_DIAG_MM=1` follow-up run to compare
`0x7fef60030000` against real `copy_one_group` reservation-group boundaries failed to launch this
pass (`.wfgy/de_only_pass45c_diag.log`, PowerShell `NativeCommandError`, "another ... release the
lock" -- a leftover `litebox_runner` process was still holding a file lock; host RAM had fallen to
~3-4.5GB free by then). Not re-attempted this pass.
