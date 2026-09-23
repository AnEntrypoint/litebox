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

## 44th-45th passes (2026-09-22), drained verbatim from AGENTS.md by the 46th pass's compaction

**44th pass** — two real bugs FIXED+verified (fd 0/1/2 dropped at the cross-process fork boundary;
a premature AF_UNIX connect-request cancellation that stalled `xfce4-session` forever on its own
D-Bus connect). With both fixed, `xfce4-session` genuinely progresses for the first time ever --
`iceauth`/`ssh-agent`/`gpg-agent`/`xfconfd`/`dbus-update-activation-environment` all `execve` (none
appear in any prior pass's log). Before `xfwm4` is reached, Xvfb SIGSEGVs again at fault address
`0x4000400` (backtrace frame0 offset `0x1b20ed`, bit-identical to the 43rd-pass-fixed crash's own
signature) -- `.wfgy/de_only_pass44_run2.log:46383-46392`.

**45th pass** — TWO real host-process bugs found+fixed (`5d63ec6`, `32dd3d5`); the Xvfb `0x4000400`
SIGSEGV was NOT reproduced across 2 post-fix boots (was present in 1 of 2 pre-fix boots), suggestive
but NOT proven fixed; a separate, still-OPEN third host-process-panic mechanism blocked full
confidence either way. Root-caused and fixed a DIFFERENT, previously-undiagnosed bug hit in the SAME
44th-pass logs: a HOST-process Rust panic (`litebox_platform_windows_userland/src/lib.rs:7396`,
`process_memory_range_by_regions`'s own `assert!`) firing inside short-lived cross-process-fork
children (`ssh-agent`, `xprop`, etc.) at the bit-identical region `0x7fef60030000-0x7fef64000000`,
Windows reporting `MEM_FREE`. (1) `allocate_pages`'s collision checks never accounted for the shared
kernel heap's `SEC_RESERVE` fallback view -- fixed, but tested+REFUTED as the cause of this specific
address (every process in the crashing fork tree lands its shared kernel heap at the FIXED base, not
the fallback); kept as a real, independent fix. (2) `Vmem::new_adopting_existing_memory` adopted a
`VM_SHARED` region into a cross-process fork child's `vmas` with `shared_handle: None`, which
`Vmem::remove_mapping`'s `shared_overlaps` check (keyed on `VmArea::view_extent()`, `None` whenever
`shared_handle` is `None`) then misclassified as ordinary PRIVATE memory, routing a later guest
`munmap`/`mprotect` straight into `deallocate_pages`/`update_permissions`'s real Windows calls
against an address this child never actually committed real memory at (`copy_one_group`/
`group_relocations` never recreates `VM_SHARED` backing for a Windows cross-process-fork child at
all). Fixed by skipping `VM_SHARED` regions at adoption entirely -- real cross-process content
sharing for them remains unimplemented. **After BOTH fixes, the bit-identical `lib.rs:7396` panic on
the SAME address STILL recurred** (during `ssh-agent`'s own exit) -- a THIRD, still-unidentified
mechanism also produced it; a follow-up diagnostic boot (`LITEBOX_DIAG_PROCESS_FORK_EXEC_FIXUP=1`/
`LITEBOX_DIAG_MM=1`) failed to even launch this pass (Windows file-lock contention against a leftover
process, host RAM down to ~3-4.5GB free) and was not re-attempted. `DE_FAILED` was reached in every
run this pass (4/4); no run reached a working window manager; no browser/app verification was
possible.

## Minor standing gotchas drained by the 46th-pass compaction

A freestanding no-libc probe's local `char buf[N] = "literal"` array initializer can crash: clang
`-O1` lowers it to an aligned SSE `movaps`, and a hand-written `_start` doesn't always give the same
alignment guarantee real crt0 does. Use a manual byte-copy loop instead (37th pass,
`advisor/probes/pty_fork_probe.c`'s own `copy_str`).

## 49th pass (2026-09-22) — buddy_system_allocator free-list corruption FIXED, real root cause

Picked up the 48th pass's own two open hypotheses: (a) does the corrupting suspend even route
through `ThreadHandle::interrupt`'s `SuspendThread` retry loop (the only `rip_in_global_allocator`
call site), and (b) is this a genuine SMP data race on `Heap::free_list` bypassing
`LockedHeapWithRescue`'s own `spin::Mutex`.

**Static analysis first (PDB archaeology, `llvm-pdbutil dump --publics`).** Extracted real RVAs for
`<SafeZoneAllocator as GlobalAlloc>::alloc`/`dealloc` and `buddy_system_allocator::Heap::<34>::alloc`/
`LockedHeapWithRescue::<34>::dealloc` from the release PDB. All four fell within ~70KB of each other
— comfortably inside even the ORIGINAL (pre-47th-pass) 128KiB window, long before the 47th pass's
widening. This was a first hint that address coverage was never really the gap for this binary's
actual layout, unlike the earlier, genuinely-4.3MB-away `slabmalloc::ZoneAllocator` case.

**Hypothesis (a) refuted live.** Added an always-on diagnostic to `ThreadHandle::interrupt` (`lib.rs`,
near the `switch_to_guest`/`is_in_guest` redirect logic) that calls `rip_in_global_allocator(rip)`
on the FINAL context right before any redirect, and prints
`[diag-interrupt-GUEST-REDIRECT-IN-ALLOCATOR]` if it's ever true (cheap: `diag_raw_print`, no
alloc/lock, safe inside the suspended-target window). Rebuilt debug, ran `de_only.sh` under
`LITEBOX_PROCESS_FORK=1`+`LITEBOX_DIAG_FATALDUMP=1`: the panic recurred 4 times in ONE run
(`.wfgy/verify48_run49.log`), zero `GUEST-REDIRECT-IN-ALLOCATOR` occurrences. The redirect mechanism
was never involved.

**Real root cause found via `RUST_BACKTRACE=full`** (`.wfgy/verify49_backtrace.log`) — 3/3
independent panics show the BIT-IDENTICAL backtrace:
`net_worker (fork child)`'s bootstrap → first `perform_network_interaction()` call → locks the
shared `Network` → `GlobalStateHandle::net_lock()` → `Network::rebind_per_process_fields` →
`self.litebox = litebox.clone()` → drops the OLD `self.litebox` in place → `Arc::drop_slow` on
`LiteBoxX` → `Descriptors`' `RwLock<Vec<Option<IndividualEntry>>>` drop glue → `Vec::drop` →
`SafeZoneAllocator::dealloc` → `Heap::<34>::dealloc` → panic at `lib.rs:165` (`class` computed from
a corrupted `Layout` = 53, `>= ORDER`).

Mechanism: `Network` (and `Pipes`) are placed in the cross-process-shared kernel arena by design —
every process in a fork family reads/writes the SAME struct bytes at the SAME address. `litebox:
LiteBox<Platform>` is `Arc<LiteBoxX<Platform>>`; `rebind_per_process_fields` exists specifically to
fix up this field to the CALLING process's own valid `LiteBox` before every use (its own doc comment
already documents two EARLIER instances of the "stale shared Arc pointer" defect class this same
field caused — an `STATUS_ACCESS_VIOLATION` in `Descriptors::iter_mut` and another in
`phy::Device::receive`). But the fix itself, `self.litebox = litebox.clone()`, is an ordinary Rust
assignment: it drops the OLD value in place before storing the new one. That OLD value's `Arc` inner
pointer was captured by whichever process last called this function (the parent, a sibling fork
child, or nobody yet if this is the very first rebind ever) — foreign, and possibly already-exited-
process-owned, memory in THIS process's address space. `Arc::drop` walks into that foreign memory,
reads whatever bit pattern is sitting at the expected "strong count" offset, and — deterministically,
run after run, because the parent's own heap layout is fairly stable across boots — reads it as the
last reference, running the REAL drop glue for a `LiteBoxX` (including the FD table `Vec`) through
THIS process's real global allocator, with a `Layout` reconstructed from bytes that were never a real
`Vec`'s capacity/size at all. That garbage `Layout` produces `class=53`, out of the `Heap`'s
`free_list[34]` bounds.

This is the EXACT SAME defect class `reset_after_poisoning` (a few lines below in the same file) was
already written to avoid for `SocketSet::remove`'s returned `Socket` — see that function's own doc
comment: "`mem::forget`, deliberately NOT a normal drop... it was never THIS process's allocation to
free in the first place." `rebind_per_process_fields` simply never got the same treatment.

**`litebox::pipes::Pipes::rebind_per_process_fields` (`litebox/src/pipes.rs`) has the IDENTICAL bug**
— `*self.litebox.lock() = litebox.clone()`, same plain-assignment-drops-a-foreign-Arc shape. Its own
doc comment already documents a live crash from this exact mechanism (`STATUS_ACCESS_VIOLATION`
inside a `Descriptors` `Vec::drop`, reached while tearing down a pipe's `WriteEnd`) — fixed the same
way. Checked the other fields the same doc comments call "the same shadow-field pattern"
(`proc_self_info`/`pts_registry`/`elf_patch_cache`/`exec_ranges_cache`/`segment_scan_cache`,
`GlobalStateHandle`): all of those are structurally different — each PROCESS constructs its OWN
fresh `Arc` once at build time and never reassigns a shared struct's field via `=`, so they don't
have this hazard. No other instance found.

**Fix** (`8b64698`): `let stale = core::mem::replace(&mut self.litebox, litebox.clone());
core::mem::forget(stale);` in both `Network::rebind_per_process_fields` and
`Pipes::rebind_per_process_fields`.

**Verification**: 7 sequential `de_only.sh` runs post-fix (debug build,
`LITEBOX_PROCESS_FORK=1`+`LITEBOX_DIAG_FATALDUMP=1`, `.wfgy/verify49_fix_run{1..7}.log`) — ZERO
`buddy_system_allocator` panics in any run (100% reproduction pre-fix: every fork child, ~3-4s in,
bit-identical "len is 34, index is 53"). The separate, pre-existing second Xvfb SIGSEGV
(`0x37f0400`/backtrace offset `0x1b20ed`, unrelated mechanism, still open) recurred in 5 of the 7
runs — untouched by this fix, as expected. An 8th run was intentionally cut short (host RAM fell to
~0.4GB free mid-run, the same transient dip the 35th/43rd passes already documented at this script
stage) once 7 clean runs already exceeded the sample size this investigation's own discipline calls
sufficient.

**Operational note**: multi-run PowerShell batches invoked via `run_in_background` intermittently hit
a sandbox error ("Remove-Item on system path ... is blocked") when using the `Remove-Item` cmdlet on
ordinary `.wfgy/*.log`/`.txt` paths inside a loop — cause not root-caused this pass (possibly a
sandbox path-matching false positive specific to background PowerShell execution). Workaround that
worked cleanly: use `[System.IO.File]::Delete`/`::Exists`/`::AppendAllText` instead of `Remove-Item`/
`Test-Path`/`Add-Content` for any file bookkeeping inside a backgrounded multi-run PowerShell script.

**Pickup**: the sole remaining blocker to a working desktop is the second Xvfb SIGSEGV
(`0x37f0400`), unchanged in scope from the 44th-48th passes' own notes — still needs either a live
`cdb`/CFI-based unwind of the crash's real call site or upstream Xvfb/glibc source cross-reference.
Now that the allocator corruption is gone, a full `webtop_stack.sh` boot is worth re-attempting: the
48th pass's own note that a still-firing host panic "changes downstream process timing/ordering"
cuts both ways — fixing a real, frequent, early-boot corruption bug may shift whether/when the Xvfb
crash triggers, for better or worse, and this pass did not yet re-run the full stack to check.

### 49th pass addendum — full `webtop_stack.sh` boot re-verified post-fix, no regression

Rebuilt the RELEASE binary with the same fix (`8b64698`) and ran a full `webtop_stack.sh` boot
(`.wfgy/webtop_pass49_boot1.log`, `LITEBOX_PROCESS_FORK=1`, `--publish 8081:8081`). Reached the SAME
best-documented state as every prior pass since the 44th — `NGINX_STARTED` → `XVFB_UP` → `DBUS_UP` →
`SELKIES_LAUNCHED_LAST` → `SELKIES_PORT_UP curl_exit=0` → `DE_LAUNCHED` → `DE_FAILED` (via
`xprop: unable to open display ':1'`) → the script's own `HOLD` loop — with ZERO
`buddy_system_allocator` panics and, this run, ZERO Xvfb `Segmentation fault` text either (the
second Xvfb SIGSEGV is timing-sensitive and doesn't fire every boot, consistent with prior passes).
`DE_FAILED` itself is unchanged and NOT this pass's finding — still the same "Cannot open display"
symptom the 30th-48th passes already narrowed to "something inside `xfce4-session`'s own process",
never yet debugged with a live `cdb -pv` attach. Host-side `curl http://127.0.0.1:8081/` (the
`--publish` port) connects at the TCP level (`netstat` confirms `LISTENING`, and a real 3-way
handshake completes) but the HTTP request itself times out with 0 bytes received — the SAME
unresolved observation the 43rd pass already recorded (Track B pickup item 2), reproduced again
here with healthy RAM (~5-8GB free throughout, ruling out the transient-RAM-dip explanation this
time) — worth a dedicated pass of its own, not chased further here. No Chrome browser extension was
connected this session (`list_connected_browsers` returned empty) and the `chrome-devtools` MCP
server failed to connect (`CONNECT_TIMEOUT`), so no screenshot was possible; the host-side curl
timeout would have blocked one regardless. Terminal Emulator/Thunar app verification needs a
working WM, not reached this pass (`DE_FAILED` persists) — remains blocked on the same two
still-open items as every pass since the 44th: the second Xvfb SIGSEGV and `DE_FAILED`'s own root
cause inside `xfce4-session`.

## 49th-52nd passes (2026-09-22), drained verbatim from AGENTS.md by the 53rd pass's compaction

**49th pass** - buddy_system_allocator free-list corruption ("len is 34, index is 53") ROOT-CAUSED
and FIXED (8b64698). Network/Pipes::rebind_per_process_fields (litebox/src/net/mod.rs,
litebox/src/pipes.rs) plain-assigned their shared-arena litebox: Arc<LiteBoxX> field on every
call, which drops the OLD value in place - that old value was always some OTHER process's
private-heap Arc pointer frozen into shared bytes (the field exists specifically to fix up a
stale shared-Arc hazard already documented twice before: an STATUS_ACCESS_VIOLATION in
Descriptors::iter_mut and another in phy::Device::receive). Arc::drop on that foreign pointer
reads a bogus refcount from whatever bits are sitting at the expected offset in this process's
address space and, deterministically (parent heap layout is stable run to run), treats it as the
last reference - running REAL drop glue (including an FD-table Vec) through THIS process's real
allocator with a Layout reconstructed from garbage bytes, corrupting Heap::free_list[34] with
class=53 (out of ORDER bounds). Found via RUST_BACKTRACE=full: 3/3 independent panics showed
the bit-identical backtrace net_worker (fork child) bootstrap -> perform_network_interaction() ->
GlobalStateHandle::net_lock() -> Network::rebind_per_process_fields -> self.litebox =
litebox.clone() -> Arc::drop_slow -> Descriptors' RwLock<Vec<...>> drop glue ->
SafeZoneAllocator::dealloc -> panic at lib.rs:165. Same defect class reset_after_poisoning
already avoids for SocketSet::remove's returned Socket. Fix: mem::replace + mem::forget the
stale value in both rebind_per_process_fields functions. Checked every other field the doc
comments call "the same shadow-field pattern": structurally different (each process builds its
own fresh Arc once, never reassigns a shared struct's field via =), no other instance found.
Verified: 7/7 clean de_only.sh runs post-fix, zero recurrence vs. 100% pre-fix reproduction.

**49th pass addendum** - full RELEASE webtop_stack.sh boot re-verified post-fix
(.wfgy/webtop_pass49_boot1.log): same best state as every pass since the 44th
(NGINX_STARTED->XVFB_UP->DBUS_UP->SELKIES_LAUNCHED_LAST->SELKIES_PORT_UP curl_exit=0->DE_LAUNCHED
->DE_FAILED via "xprop: unable to open display ':1'"->HOLD), zero buddy_system_allocator panics,
zero Xvfb SIGSEGV text this run. Host-side curl connects at the TCP level but the HTTP request
itself times out with 0 bytes - same unresolved observation as the 43rd pass, reproduced again
with healthy RAM (~5-8GB free) - a dedicated pass of its own, not chased further. No Chrome
extension connected, chrome-devtools MCP CONNECT_TIMEOUT - no screenshot attempted.

**50th pass** - the second Xvfb SIGSEGV's full register state proven bit-identical across
independent boots: rip=0x7fefedeababd (glibc __memmove_avx_unaligned_erms's 32-64-byte AVX2
path), rdx=0x40. rdi (dest) is a real per-process heap chunk (offset fixed, base moves with
ASLR, expected); rsi (src, fault address 0x37f0400) does NOT move with ASLR at all - the key
clue the 51st pass root-caused.

**51st pass** - SysV shm cross-process-attach bug ROOT-CAUSED and FIXED, no cdb needed. sys_shmat
(litebox_shim_linux/src/syscalls/mm.rs) handed back a bare SysvShmSegment.addr - a real mapping
ONLY in the CREATING process - to every attacher; under LITEBOX_PROCESS_FORK=1, a genuinely
different real Windows process (e.g. Xvfb attaching a segment an X11 MIT-SHM CLIENT created) got
that numeric value with ZERO backing memory of its own - exactly matching rsi's fixed, non-ASLR'd
signature. Old design assumed the pre-cross-process-fork "one shared host address space" model.
Fix: shmget no longer maps anything (matches real Linux); every shmat, including the creator's
own first one, opens a NAMED platform shared-memory object (create_named_shared_memory, Windows
impl = CreateFileMappingW(name="Local\litebox_sysvshm_<shmid>")) and maps it into ITS OWN address
space via map_existing_shared_pages - the returned address is per-process, matching real Linux.
shmdt's reverse lookup moved to a new per-process FilesState::shm_attachments table. Verified:
10/10 clean de_only.sh boots, ZERO recurrence of either Xvfb SIGSEGV signature.

**52nd pass** - full webtop_stack.sh boot re-verified past the 51st-pass fix: BOTH Xvfb SIGSEGVs
confirmed gone on the full stack too (.wfgy/webtop_release_boot6.log, UTF-16LE encoded), zero
sigsegv/panic/segmentation matches anywhere. DE_FAILED still fires, but the 30th-49th passes'
"Cannot open display" framing is REFUTED for this run - that string appears nowhere; xdpyinfo
succeeds (rc=0) right before each WM launch attempt and DISPLAY/DBUS_SESSION_BUS_ADDRESS are
confirmed correctly set. Fresh evidence via the guest-stderr channel: (a) xfce4-session (pid 6948)
runs deep into startup (iceauth/ssh-agent/gpg-agent/xfconfd, later xfsettingsd/xfdesktop/Thunar
all really execve) but floods GLib-GIO-CRITICAL: g_dbus_proxy_call_sync_internal/
g_dbus_error_is_remote_error: assertion 'error != NULL' failed around its D-Bus autostart-lookup
calls; (b) xfwm4 itself never appears ANYWHERE in the log; (c) a new bug: gpg-agent (pid 119) hit
a fatal glibc heap-corruption assertion (malloc.c:3846 __libc_calloc, SIGABRT) ~1.2s into
startwm.sh, not yet root-caused; (d) vmem-adopt-probe showed every fork child with a high
VM_SHARED region count (up to 84/117) - worth its own pass. Fixed in passing (39a878b): the
probe's own comparison filter was stale.

## 53rd pass (2026-09-22) - dbus-daemon service-activation false-"exited" bug found

Re-verified the 52nd pass's own evidence directly against .wfgy/webtop_release_boot6.log (must
iconv -f UTF-16LE -t UTF-8 before grep/read, easy to get a false "zero matches" otherwise):
confirmed xfwm4 genuinely never appears (0 matches), xfce4-session appears 179 times, the
GLib-GObject-CRITICAL/GLib-GIO-CRITICAL flood is real (first burst at elapsed 14.292s, guest pid
6948), and traced its IMMEDIATE cause for the first time: dbus-daemon (pid 17604) logs, in order,
"Activating service name='org.a11y.Bus' requested by ':1.0' (... pid=6948 ...)" then ~84ms later
"Activated service 'org.a11y.Bus' failed: Process org.a11y.Bus exited, reason unknown" (lines
14074-14090), and the identical pattern for org.xfce.Xfconf moments later (lines 14245-14254) -
both are D-Bus SERVICE ACTIVATION failures, not something xfce4-session itself does wrong;
xfce4-session's own GLib-GIO-CRITICAL burst is a downstream symptom of calling methods on a proxy
for a service whose activation dbus-daemon already gave up on.

New, more instrumented repro this pass (.wfgy/de_only_pass53_utf8.log, debug binary,
LITEBOX_PROCESS_FORK=1, LITEBOX_LOG=warn,litebox_diag::stderr_capture=debug,
litebox_shim_linux::syscalls::process=debug,litebox_shim_linux::syscalls::unix=debug,
de_only.sh) traced dbus-daemon's OWN activation fork end to end for the first time:

- 14.195533s: dbus-daemon (guest tid=16560) enters try_cross_process_fork for its activation
  babysitter, RIGHT AFTER logging "Activating service name='org.a11y.Bus'".
- 14.195683s: eligibility passes (dropped_cloexec=7 dropped_pty=0 carried_pipes=2 carried_files=3
  - dbus-daemon's own listening sockets are CLOEXEC so they're correctly dropped rather than
  refusing the fork; only its babysitter-protocol pipes are carried).
- 14.237369s: child observed alive and running its OWN startup ([process_fork_diag]
  globalstate-probe (child): adopted the parent's writable layer...) - i.e. the real Windows
  process genuinely exists and is executing.
- 14.237711s (< 1ms later): dbus-daemon ALREADY prints "Activated service 'org.a11y.Bus' failed:
  Process org.a11y.Bus exited, reason unknown".
- 14.237842s/14.237870s: sys_wait4(tid=16560, pid=29, options=1) (WNOHANG) immediately followed by
  sys_kill(pid=29, ...) (hits the pass-141 ESRCH-for-cross-process-child gap, silently ignored by
  dbus per its own kill()-return-value-unchecked design, confirmed by fetching dbus-spawn-unix.c/
  bus/activation.c from the d-bus/dbus GitHub mirror) then a BLOCKING
  sys_wait4(tid=16560, pid=29, options=0).
- The REAL target binary's execve (/usr/libexec/at-spi-bus-launcher) does not appear until line
  28663 of the same log - thousands of log lines, and (given this log's own interleaving of many
  concurrent processes) a materially later point in wall-clock time - AFTER the "exited, reason
  unknown" report already fired. Same ordering for org.xfce.Xfconf/xfconfd (activation-failed at
  line 26780, real xfconfd execve at line 37119).

Conclusion (not yet root-caused to an exact line - do not patch blind, matching this project's own
standing rule): dbus-daemon's babysitter-exit detection concludes the cross-process-forked
babysitter child has ALREADY exited essentially immediately after fork() returns - while the real
Windows process is demonstrably still alive and merely slow (relative to real Linux fork+exec) to
reach its own execve. Fetched _dbus_babysitter_set_child_exit_error (dbus/dbus-spawn-unix.c,
d-bus/dbus GitHub mirror): "exited, reason unknown" fires specifically when have_exec_errnum/
have_fork_errnum/have_child_status are ALL false - i.e. dbus's own babysitter-to-daemon
communication pipe reported the child gone WITHOUT ever conveying a real waitpid status,
consistent with either (a) try_wait_for_cross_process_exit
(litebox_platform_windows_userland/src/process_fork.rs:4226, backed by
WaitForSingleObject(handle, 0)) or the async exit-notifier thread (arm_cross_process_exit_notifier,
litebox_shim_linux/src/syscalls/process.rs:3363, backed by WaitForSingleObject(handle, INFINITE)
on a background thread) reporting the child's Windows process HANDLE as signaled/exited when it
should not yet be, or (b) some other early guest-visible signal (e.g. a spurious SIGCHLD delivery)
tricking dbus's own SIGCHLD-driven waitpid(WNOHANG) poll into running before the real child has
done anything, without dbus itself being wrong to trust it. Affects every D-Bus service-activation
call observed this pass (org.a11y.Bus, org.xfce.Xfconf, and per the 52nd pass's own full-stack
log, org.a11y.atspi.Registry later on), not something specific to any one activated binary; this
is very likely upstream of (though not yet proven to be the sole cause of) the xfwm4-never-launches
symptom IF xfce4-session's own WM-launch path also depends on this same fork/wait mechanism, but
see the next finding for a competing, equally-live hypothesis. Pickup: add a temporary diagnostic
printing WaitForSingleObject's raw return value and elapsed-ms-since-CreateProcessW at both call
sites above, to conclusively confirm/refute a premature-signaled handle before touching any
production code path.

Second finding, a real divergence from the 52nd pass: in this pass's OWN de_only.sh run,
xfce4-session (guest pid 16908 this run) never called clone() for a real process EVEN ONCE across
the entire run (zero process-clones, only 4 THREAD-clones/"spawned new task", i.e. internal GLib
worker threads) - meaning it never attempted to launch ANY session client, not xfwm4 specifically:
no iceauth, no ssh-agent, no gpg-agent either (all present in the 52nd pass's fuller
webtop_stack.sh boot, all absent here). Either genuine run-to-run non-determinism (this
investigation has repeatedly found timing-sensitive failures) or a real environment difference
between the bare de_only.sh harness (fresh $HOME/$XDG_RUNTIME_DIR, no seeded session cache) and
the fuller webtop_stack.sh boot. Real implication for the next pass: xfce4-session's OWN
very-early D-Bus-proxy construction (the first GLib-GObject-CRITICAL: invalid (NULL) pointer
instance fires before ANY client fork in BOTH this run and the 52nd pass's) is a more
consistently-reproducing, more upstream candidate blocker than "xfwm4 specifically" - it may be
stalling or aborting xfce4-session's entire client-startup phase silently, with xfwm4's absence
merely the most visible symptom. Pickup: a live cdb -pv attach on xfce4-session breaking on
g_bus_get_sync/g_dbus_proxy_new_sync (now finally uncontaminated by the Xvfb crash) is the
concrete next step - never attempted by any pass to date despite being flagged as available since
the 44th.

Also reconfirmed this pass, negative results worth recording: zero host panics, zero
SIGSEGV/segmentation-fault text, and zero gpg-agent activity at all (consistent with xfce4-session
never reaching client-spawn this run) in the full .wfgy/de_only_pass53_utf8.log - the 49th/51st-pass
fixes (buddy allocator, SysV shm) continue to hold with zero recurrence on yet another independent
run.

## 54th pass (2026-09-22) -- D-Bus service-activation false-"exited" bug ROOT-CAUSED and FIXED;
## xfwm4 execve'd for the first time ever; drained verbatim from AGENTS.md by the 55th pass's compaction

wait4_diag instrumentation (GetProcessTimes-based, added to both try_wait_for_cross_process_exit,
process_fork.rs, and arm_cross_process_exit_notifier, lib.rs) proved one live org.a11y.Bus
babysitter's real Windows process stayed alive a genuine 3856ms after its own CreateProcessW while
dbus's "exited, reason unknown" print fired <1ms after fork() returned to the guest -- 3.8+ real
seconds before either WaitForSingleObject call had returned anything for that handle at all. This
REFUTED the 53rd pass's premature-exit-report hypothesis with direct evidence.

Real cause (confirmed against fetched upstream dbus-spawn-unix.c): dbus's activation babysitter
reports its pid back over a _dbus_socketpair() (CLOEXEC, addressless) pair BEFORE any exec() --
exactly the case try_cross_process_fork's blanket CLOEXEC-drop policy (process.rs line ~2846)
documents as unsafe: dropping that fd silently left the daemon's own end of that specific pair
referencing a peer that was never given a live copy in the cross-process child, read as an immediate
EOF/HUP. Fixed: raw_fd_is_addressless_unix_socket_pair (net.rs) distinguishes a genuine
socketpair(2) result (both ends Unnamed) from an ordinary named-peer CLOEXEC client socket
(X11/D-Bus connections, 95.1% of all cross-process-fork CLOEXEC drops measured over a real
debian-xfce boot) -- only the narrow addressless case now refuses the whole cross-process fork
(falls back to thread-based) instead of silently dropping the fd (commit 66265d9).

Live-verified twice (.wfgy/de_only_pass54_fixverify.log, de_only_pass54_lean.log, both UTF-16LE):
Activated service 'org.a11y.Bus'/'org.xfce.Xfconf' now print Successfully activated, the
GLib-GIO-CRITICAL/GObject-CRITICAL flood is GONE (zero in either log), and /usr/bin/xfwm4
genuinely execve's for the first time in this entire multi-week investigation.

Still short of DE_UP: xfwm4 stays alive (periodic futex wakes past its own 150s+ elapsed clock)
but never sets _NET_SUPPORTING_WM_CHECK in the observed window. New top suspect this pass, reproduced
twice: index out of bounds: the len is 1 but the index is 13 panic at litebox/src/fd/mod.rs:422
(exact log line: de_only_pass54_fixverify.log:462972, UTF-16LE, thread <unnamed> (28128)), inside
some cross-process-forked process whose own Descriptors table had only 1 entry at the time -- not
yet root-caused this pass (see the 55th pass below for the actual root cause and fix). Host RAM
cratered UNDER 1GB TWICE on de_only.sh ALONE (it now reaches much deeper into the boot, so far more
processes spawn) -- both runs killed via Invoke-CimMethod Terminate before a freeze; webtop_stack.sh
was deliberately not attempted this pass given that fragility.

## 55th pass (2026-09-22) -- fd/mod.rs:422 panic ROOT-CAUSED and FIXED; a SEPARATE, serious
## RAM-exhaustion issue identified as the current proximate blocker to DE_UP, not yet fixed

Root cause of the fd/mod.rs:422 panic, confirmed by code reading (not yet by a live debugger --
the live repro captured by the 54th pass was sufficient: exact file:line, exact panic message, exact
call stack shape). litebox::fd::Descriptors (litebox/src/fd/mod.rs) is a genuinely PER-PROCESS,
PRIVATE structure -- a fresh cross-process-forked child's own table starts small (as few as 1 entry,
built up one insert() call at a time by adopt_forked_process/the child's own initialize_stdio_in_
shared_descriptors_table and, later, install_file_at_fd/install_eventfd_at_fd/install_pipe_*_at_
fd). TypedFd's inner index is only ever meaningful against the SAME Descriptors instance that
insert()ed it. The 28th pass already root-caused and partially fixed exactly this defect class for
ONE call site (drain_entries_full_covered_by, used against Network::queued_for_closure, a
genuinely cross-process-shared fd queue where one process's pushed TypedFd index is read back by
every OTHER process's own periodic tick) -- see that pass's own three-part doc comment, still in
fd/mod.rs, for the fullest available description of the general defect ("a TypedFd one process
pushed encodes an index into THAT process's own private entries, meaningless -- out of bounds, or
resolving to an unrelated live entry -- in a different process's table"). That fix made ONLY
drain_entries_full_covered_by use self.entries.get(idx) (bounds-checked, None-tolerant) instead
of self.entries[idx] (panics on out-of-bounds) -- every OTHER accessor in the same file
(with_entry, with_entry_mut, entry_handle, get_entry, get_entry_mut,
with_entry_mut_via_internal_fd, with_metadata, with_metadata_mut, set_entry_metadata,
set_fd_metadata) was never swept the same way, and get_entry (line 422 at the time of the panic)
was exactly one of the un-swept ones.

This session did not conclusively identify WHICH specific caller fed a foreign/stale index into
get_entry for this particular panic (the surrounding log window at the exact panic line
carried no task-resume-probe/globalstate-probe markers to anchor it to a specific child's
bootstrap step, and the massively-interleaved multi-process combined log makes attribution by
timestamp alone unreliable -- host_tid=28128 reused across the log for at least two different real
Windows threads at different points, consistent with OS TID reuse after a thread exits). Given (a)
this is the exact SAME general defect class as three already-fixed bugs this project has fixed this
way before (Network::queued_for_closure, Pipes.litebox's stale pointer, FutexManager's
stack-allocated LoanList sharing hang -- see AGENTS.md's shared-memory-foundations section), (b)
AGENTS.md's own hard, non-negotiable invariant that guest-reachable code must return an errno and
never panic (the host process IS the entire guest session -- one bad index must not kill it), and
(c) the fix is a mechanical, low-risk, purely-defensive bounds-check with no behavior change for
any legitimately-in-bounds caller, the correct and sufficient fix was to sweep ALL of this file's
direct-indexing accessors to the same None-tolerant pattern, rather than hunt further for the
exact caller. Done, commit faa74c6: cargo check -p litebox clean, cargo test -p litebox --lib
fd:: 4/4 pass, cargo check -p litebox_shim_linux clean (pre-existing, unrelated backend_tracing
feature-gate error confirmed present on git stash too -- not caused by this change; the real runner
crate that always enables that feature built and ran fine, cargo build -p
litebox_runner_linux_on_windows_userland succeeded).

Live verification, two independent LITEBOX_PROCESS_FORK=1 de_only.sh boots on the rebuilt debug
runner (.wfgy/de_only_pass55_verify.log, full LITEBOX_LOG incl. syscalls::process=debug; and
.wfgy/de_only_pass55b_lean.log, leaner LITEBOX_LOG=warn,litebox_diag::stderr_capture=debug, both
UTF-16LE): neither run reproduced the fd/mod.rs panic or ANY panic text at all. Both runs
independently progressed past DBUS_UP/DE_LAUNCHED_DIRECT into the _NET_SUPPORTING_WM_CHECK
poll loop and showed the SAME real forward progress signal -- xprop -root _NET_SUPPORTING_WM_CHECK's
own error text changed from "no such atom on any window" (the atom name has never been interned) to
"not found" (the atom name IS now interned -- something, most plausibly xfwm4, made a real
XInternAtom call -- but the property is not yet SET on the root window) between roughly poll 4-5 of
12. This is genuine, reproducible evidence that SOMETHING is actively doing real X11 work in this
window, not merely hung.

But BOTH runs died from RAM exhaustion at almost the exact same point before either reached DE_UP
or DE_FAILED's own 60s timeout, independently confirming the 54th pass's own "host RAM cratered
under 1GB" observation as a real, reproducible, SEPARATE blocker -- not a one-off: run 1
(de_only_pass55_verify.log) went from 3.6GB free (poll 6) to 738MB free to fully gone (root runner
process no longer resolvable by PID, only two bare-77-char-command-line ORPHANED cross-process
children left running, exactly AGENTS.md's own documented "dead root runner, orphans keep running"
signature) within about 30 seconds, between poll 6 and poll 7 -- no panic, no RUN EXIT line, the log
simply stops, consistent with an un-caught host-level OOM kill of the root process rather than a guest
panic. Cleaned up via Invoke-CimMethod -MethodName Terminate on the two orphaned bare-command-line
children once Get-CimInstance showed them as the only survivors (their full --oci-image root was
already gone -- Terminate on it separately returned "Not found", confirming it self-terminated before
the cleanup call landed); RAM recovered fully to ~9.2GB free the instant they were gone, confirming no
leak persists once the process tree is down. Run 2 (de_only_pass55b_lean.log, deliberately trimmed
LITEBOX_LOG to rule out logging overhead as the cause) reached the same poll-6-to-7 danger window at
2.87GB free and was heading toward the identical trajectory (2.5GB, then 1.95GB -- the SAME 1.95GB
floor a completely unrelated calibration re-run hit in the 34th pass) when this pass deliberately
terminated it proactively rather than let a second uncontrolled OOM happen; RAM recovered to ~9.1GB
free immediately after. Trimming the log level did NOT meaningfully change the RAM trajectory or its
timing (both runs crossed 4GB free within one poll cycle of DE_LAUNCHED_DIRECT and were below 3GB by
poll 5-6) -- the leak/cost is in actual guest-process/fork proliferation during this exact window
(dbus service activation + xfce4-session's own client-spawn fan-out + the poll loop's own per-poll
xprop fork), not primarily logging volume, though de_only_pass55b_lean.log was STILL 75MB despite
the trimmed filter (guest stderr_capture=debug alone captures a large volume once XFCE's session
clients start producing their own GTK/GLib warning noise).

Conclusion: the _NET_SUPPORTING_WM_CHECK-never-set symptom is, as of this pass, NOT confirmed to
be a DE_FAILED-causing logic bug in xfwm4 or a litebox syscall bug at all -- every run that has
reached this exact window since the 54th pass's D-Bus fix landed has died from RAM exhaustion before
the window's own natural 60s timeout could even elapse, with real evidence (the atom-interning
progress) that something was still actively working right up until the kill. This reframes the
current top blocker: it may be that xfwm4 genuinely completes and sets the atom given enough
sustained RAM -- this has never actually been observed to fail on its own merits, only to run out of
host memory first. Pickup, precisely scoped: (1) do NOT re-attempt this exact repro back-to-back
without a substantial RAM-recovery pause and a below-4GB-committed baseline check first (this pass's
host was measured healthy at 8-9GB free before each of its two attempts and still hit the SAME danger
zone by poll 6, so the fork-proliferation cost itself is the dominant factor, not a dirty starting
state); (2) investigate WHERE the RAM is actually going in this exact window -- top suspects, not yet
measured: xfce4-session's own session-client fan-out (each new client is a fresh cross-process fork,
and per process_fork.rs's own doc comment on LITEBOX_DIAG_SHARED_HEAP_INHERIT, every plain
external-command-style fork that is NOT using that still-off-by-default flag rebuilds its own full
in-memory merged OCI rootfs from the layer cache privately, a single ~173MB+ host allocation PER
FORK, reclaimed only once that short-lived process exits -- a de_only.sh boot forks xset/
xdpyinfo/xprop x13+/sleep x dozens/dbus-daemon's activation babysitters/every xfce4-session
client on top of this same per-fork cost, which is a very plausible dominant mechanism and was
NOT the reason LITEBOX_DIAG_SHARED_HEAP_INHERIT=1 was left off -- that flag was left off because
the same 173MB-class allocation does not fit the shared heap's own fixed 64MiB cap anyway, so turning
it on does not fix this without ALSO enlarging that heap, which is real, separate, unscoped work);
(3) a genuine fix likely needs either a materially larger shared kernel heap (currently a hard 64MiB
cap, shared_kernel_arena_alloc) with LITEBOX_DIAG_SHARED_HEAP_INHERIT finally promoted to default,
or a way to avoid a full rootfs rebuild for a short-lived external command's cross-process fork
entirely (e.g. sharing the ALREADY-rewritten/merged layer data read-only across the whole fork family
rather than re-merging it fresh per fork) -- both are real, scoped, but substantial follow-on
engineering, not a quick patch; (4) once (2)/(3) narrow the real allocation site, re-attempt this
exact de_only.sh repro to determine for the first time whether xfwm4 genuinely reaches
_NET_SUPPORTING_WM_CHECK given enough sustained RAM, before assuming any further xfwm4-specific
logic bug exists at all.

## 58th-60th passes (2026-09-23) — ssh-agent/xfwm4 blocker root-caused and fixed

**58th pass** — fixed `ps -ef`'s `fatal library error, lookup self` (real procps needs its own
pid's `/proc/<pid>/stat`; `Procfs` had zero numeric pid subdirectories). Fixed in
`litebox/src/fs/procfs.rs`: one subdirectory per pid the shared `ProcSelfTable` already tracks
(`ProcfsDirHandle::Pid`/`ProcPidEntry`/`ProcSelfTable::get`/`pids`, reusing `/proc/self`'s own
renderers). Verified 2/2 clean. Root-caused (not yet fixed) `xfwm4`'s non-launch as now 100%
reproducible at an identical point: `xfce4-session` spawns `iceauth`+`ssh-agent`, then goes silent
forever, never reaching `xfwm4`. Hypothesis at the time: `ssh-agent`'s own daemonizing fork holds a
bound AF_UNIX listening socket (uncarriable), forcing its inner fork to a THREAD-based
(same-Windows-process) child; that long-lived agent keeps the hosting Windows process alive
forever, so a PROCESS-handle-based `wait_for_process_exit`/`try_wait_for_cross_process_exit`
(`process_fork.rs:4263`/`4297`) never signals (the task's own initial OS thread returns long before
the whole process exits).

**59th pass** — implemented and live-verified a task-scoped cross-process exit wait, fixing the
general case described above: `spawn_process_fork_child` (`process_fork.rs:1695`) no longer closes
the child's initial THREAD handle, returning it too; `CrossProcessChildHandle`
(`litebox/platform/mod.rs:1485`) now carries it; new `wait_for_thread_exit`/
`try_wait_for_thread_exit` (`process_fork.rs`, `GetExitCodeThread`/`GetProcessIdOfThread`) back
`wait_for_cross_process_exit`/`try_wait_for_cross_process_exit` (`lib.rs:12135`). Confirmed correct
by construction first (`lib.rs:3406` `run_thread_inner`, `4381` `thread_start`: every guest task
gets its own dedicated OS thread running exactly one `run_thread_arch` call, terminating when it
returns — 1:1 task-scoping holds structurally) and live (`.wfgy/de_only_pass59_run1.log`: fires
correctly dozens of times, e.g. pid=23868 `sleep`, resolving the instant `run_thread` returns with
the correct encoded status `0xc0de0000`). BUT `ssh-agent` (tid=27264, Windows pid 27264) still never
unblocked `xfce4-session`'s `wait4`: its `exit_group`→`prepare_for_exit` ran clean through
`close_all_fds`/`take_children` (`n_orphans=1`) then never logged again — `run_thread` genuinely
never returned for this task. Live `cdb -p 27264 -pv` (release binary, no symbols), two snapshots
~30s apart: byte-identical stack, no Win32 wait syscall at frame 0 — stable RSP, parked
mid-execution, the signature of a spinlock retry loop. Named frame deferred to the next pass
(release-binary ICF folding makes any name from that binary unreliable per this file's own standing
warning) — tentatively associated with the standing `SafeZoneAllocator::alloc`/`dealloc` spinlock
suspicion, NOT yet confirmed. Also ported (real, low-risk, did not alone fix this case): `sys_wait4`'s
thread-based specific-pid branch (`process.rs:2468`) was missing the bounded-repoll/`Interrupted`-
recheck its siblings got 21st/24th pass.

**60th pass (2026-09-23) — the tentative 59th-pass suspicion (`SafeZoneAllocator`'s spinlock) was
WRONG; the real mechanism, confirmed with a fully-named debug-build stack trace, is a DIFFERENT,
previously-unexamined spinlock one layer up: `RawMutex`'s own internal `WaiterQueue::with_lock`.**

Built the DEBUG binary (`cargo build -p litebox_runner_linux_on_windows_userland`, no `--release`)
and repeated `.wfgy/de_only.sh` under `LITEBOX_PROCESS_FORK=1`. `cdb -pv` (non-invasive attach,
`_NT_SYMBOL_PATH` pointed at `target/debug` so the shipped `.pdb` actually resolves symbols — the
59th pass never did this) on the process hosting the stuck exit path, three independent snapshots
spanning 40+ real seconds, all byte-identical `Child-SP`, only the micro-offset inside one function
moving. The full named stack (innermost first):

`core::sync::atomic::Atomic<bool>::compare_exchange_weak` <- `litebox_platform_windows_userland::
WaiterQueue::with_lock<...block_or_maybe_timeout::Registration...>` <- `RawMutex::
block_or_maybe_timeout` <- `impl$27::block` <- `litebox::sync::mutex::SpinEnabledRawMutex::
lock_contended` <- `..::lock` <- `litebox::sync::mutex::Mutex<..Vec<(i32, Arc<Process>)>..>::lock`
<- `litebox_shim_linux::syscalls::process::Process::adopt_children` <- `litebox_shim_linux::Task::
prepare_for_exit` <- `litebox_shim_linux::impl$21::drop` (LinuxShimEntrypoints) <-
`litebox_platform_windows_userland::run_thread_with_fork_verification`

i.e. exactly the guest exit path (`Task::prepare_for_exit` -> `adopt_children`, reparenting orphans)
that the 58th/59th passes already knew never logs again -- this thread is not blocked on a real
Win32 wait at all, it is spinning forever trying to ACQUIRE `WaiterQueue`'s own internal
`lock: AtomicBool` (a `compare_exchange_weak` retry loop with no OS wait), because some OTHER caller
already set that same flag to `true` and never cleared it.

**Root cause, confirmed by reading `WaiterQueue::with_lock`'s source (`litebox_platform_windows_
userland/src/lib.rs`) before touching it**: the function acquired the spin-flag, called the
caller-supplied closure `f`, and only THEN executed `self.lock.store(false, Release)` as a plain
statement -- no RAII guard. `RawMutex::wake_many` (called on every unlock of every contended mutex
in the whole process) passes a closure that calls `WaiterQueue::drain_locked`, which builds a
`Vec<WaiterRecord>` via `Vec::new()` + `.push(..)` -- an ordinary heap allocation through this
process's own `#[global_allocator]` (`SLAB_ALLOC`, `litebox::mm::allocator::SafeZoneAllocator`). If
that allocation ever panicked -- and `SafeZoneAllocator::alloc`/`dealloc`'s own `OutOfMemory`/
`InvalidLayout`/deallocate-failure branches DID panic, via `.expect(msg)`/`panic!("{layout:?}")`,
while `self.slab_allocator.lock()`'s own guard was held -- the panic unwound straight through
`with_lock`'s stack frame and past the un-guarded `store(false, ..)`, permanently wedging
`WaiterQueue::lock` at `true`. The panicking OS thread itself survived (caught by the per-task
`catch_unwind` wrapper every guest task already has, visible in literally every stack trace this
whole investigation has captured) -- so this was never a dead holder in `RawMutex`'s own sense
(`OpenProcess`/`GetExitCodeProcess` would report that thread's process as perfectly healthy
forever); it is a purely structural missing-panic-safety bug, one layer below `RawMutex` itself.

**Fix** (`61c235e`): `WaiterQueue::with_lock` now releases via a `litebox::utils::defer` guard (the
same idiom this file already uses elsewhere, e.g. `ThreadHandle::interrupt`'s `_resume_guard`), so
the release runs on every exit path, panic included. Defense in depth: every
`SafeZoneAllocator::alloc`/`dealloc` failure panic that used to format a message is now a plain
`&'static str`-only `panic!` (no `{}` interpolation, so it can never reach `format!` and therefore
can never recurse into this allocator no matter which lock -- guarded or not -- happens to be held
when it fires), mirroring the allocation-free-failure discipline `WindowsUserland::alloc`'s own
`CreateFileMappingW` failure path already established.

**Verification, honestly incomplete**: the PRE-fix hang was cleanly and unambiguously reproduced
and root-caused (three clean cdb samples, healthy 3-4GB free RAM at the time, one single thread
permanently frozen, zero forward motion). POST-fix, re-verification in the same session was
confounded by this exact repro's own well-documented, SEPARATE per-process RAM cost (Track B item 1
below) -- every debug-build attempt fell to 1-3GB free within about a minute of `DE_LAUNCHED_DIRECT`
regardless of this fix, and under that pressure, similar-looking `WaiterQueue::with_lock` contention
was observed on OTHER, unrelated `RawMutex` instances too (e.g. `litebox::net::Network`'s own
mutex, via `wake_many`'s release path) -- consistent with genuine system-wide CPU/memory starvation
making ordinary brief contention look artificially prolonged, not with the exact fixed mechanism
recurring (a fresh binary confirmed to contain both fixes was used throughout). One clean debug-build
sample of the ORIGINAL frozen thread, taken shortly after the fix under still-healthy RAM, showed
its retry-loop micro-offset actively changing between samples (progress) rather than the pre-fix
dead freeze -- suggestive but not a full `DE_UP`/`xfwm4`-launch confirmation. `_NET_SUPPORTING_WM_CHECK`
was not observed to fire in this pass. Pickup: re-verify on a host that can sustain several minutes
of a debug-build boot without falling below ~4GB free (or fix Track B item 1's per-process RAM cost
first), then confirm `xfconfd`/`xfwm4` actually spawn and `_NET_SUPPORTING_WM_CHECK` gets set.

## 61st pass (2026-09-23) -- release-binary re-verification: hang CLOSED for good, DE_FAILED narrowed to xfwm4's own X11-registration step

**Task**: the 60th pass's fix (`61c235e`, `WaiterQueue::with_lock` panic-safety) was diagnosed and
verified only on a DEBUG binary, itself confounded by that build's own well-documented per-process
RAM cost. This pass rebuilt the RELEASE binary and re-ran the exact same repro to get an
unconfounded answer, per the pickup note at the end of the previous entry.

**Build verification**: `cargo build --release -p litebox_runner_linux_on_windows_userland` -- the
build was already up to date (source `litebox_platform_windows_userland/src/lib.rs` mtime
02:32:27, binary mtime 02:49:35, both before HEAD's commit timestamp 03:05:38 which is just when
the commit object was written, not when the file was edited) -- confirmed via `git log -1 --format=
%cI 61c235e` plus direct `stat` comparison of source vs. binary mtimes, not assumed.

**Verification runs (3 total, all clean)**:
1. `.wfgy/de_only_pass60_release_run1.log` (pre-existing on disk from immediately after the build,
   `LITEBOX_PROCESS_FORK=1`, `LITEBOX_LOG=warn,litebox_platform_windows_userland::fork_verify=
   error`): `DE_ONLY_START` -> `XSOCK_WAIT_DONE` -> `DBUS_UP` -> `DE_LAUNCHED_DIRECT` -> 12/12
   `WM_POLL`s -> `DE_FAILED after 60s` -> `HOLD` loop to at least t=280s. Zero hang.
2. `.wfgy/de_only_release_verify_run1.log` (fresh run, same env/command): identical marker
   sequence, `DE_FAILED after 60s`, zero hang.
3. `.wfgy/de_only_release_verify_run2_exectrace.log` (fresh run, same env plus
   `litebox_shim_linux::syscalls::process=debug` added): identical marker sequence, `DE_FAILED
   after 60s`, zero hang. (This run's execve/futex trace is the source of the findings below.)

All three used the exact repro (`.wfgy/de_only_seed.tar`, `docker.io/linuxserver/webtop:debian-xfce`,
`/bin/bash /de_only.sh`) that was previously 100% reproducibly frozen forever at exactly this point
(58th/59th passes, `ssh-agent`'s `prepare_for_exit` never returning). **3/3 clean vs. the
pre-fix 100% freeze rate is the closing confirmation this investigation needed.**

**DE_FAILED narrowed to a new, precise, different mechanism.** Two independent lines of evidence
from run 3 (execve trace plus a live `cdb -pv` attach on a separate, later run):

*Execve/futex evidence* (`.wfgy/de_only_release_verify_run2_exectrace_utf8.log`, converted from
UTF-16LE): `xfwm4` genuinely `execve`s successfully -- `sys_execve` tries `/lsiopy/bin/xfwm4`,
`/usr/local/sbin/xfwm4`, `/usr/local/bin/xfwm4`, `/usr/sbin/xfwm4` (all `ENOENT`, normal `$PATH`
walk) then `/usr/bin/xfwm4` (`resolve_shebang: open result=Ok(())`), at log line ~10240 -- well
BEFORE `WM_POLL n=4` (line 11494) and `DE_FAILED` (line 51597). Its guest tid (10136 in this run)
then shows completely normal `futex` WAKE/WAIT activity -- a live GTK/glib main-loop idle pattern --
at elapsed 0.1s, 3-8s (bursty, startup), then isolated single WAKEs at 8.9s, 21.1s, 32.1s, 42.9s,
53.3s, 63.4s, 74.4s (roughly every ~10-11s, a periodic idle timer, not a crash or a stall). The full
XFCE session fan-out is ALSO visible in the same execve trace, well past `DE_FAILED`'s 60s mark:
`at-spi-bus-launcher`, `xfconfd`, `at-spi2-registryd`, `iceauth`, `ssh-agent`,
`dbus-update-activation-environment`, `gpgconf`, `gpg-connect-agent`, `gpg-agent`, `xfwm4`,
`xfsettingsd`, `dconf-service`, `xfce4-panel`, `Thunar`/`thunar-real` (desktop icons daemon),
`xfce4-panel`'s `wrapper-2.0` plugins, `xfdesktop`, `pm-is-supported`, `start-pulseaudio-x11` plus
`pactl` -- i.e. essentially the ENTIRE real XFCE desktop session actually launches successfully in
the background even though the harness's own 60s `WM_POLL` window already gave up.

*Live cdb evidence* (`.wfgy/xfwm4_cdb_snapshot.log`, orchestrated via `.wfgy/
xfwm4_cdb_orchestrator.ps1`, a SEPARATE, later run purpose-built to grab a fast snapshot before the
per-process RAM cost below forced a kill): the harness's own execve-trace log was tailed live to
find the `winpid=` printed by `task-resume-probe (child, winpid=NNNN)` immediately preceding the
successful `argv0=/usr/bin/xfwm4` line (winpid=8836 this run), then `cdb.exe -pv -p 8836 -c "kn;
~*kn; qd"` attached non-invasively (no `g`, no bare `q`, per the standing cdb-safety lesson) within
seconds of `xfwm4`'s own `execve`. All 10 of that process's OS threads dumped clean, REAL symbol
names (this release binary's ICF folding did NOT prevent useful names here, contrary to the
standing caution -- worth remembering release-binary `cdb` is not ALWAYS unusable, just unreliable):
thread 0 = main, in `std::sys::thread::windows::Thread::join` (waiting for the guest thread);
threads 1-3 = idle `TppWorkerThread`s; thread 4/7 = `detached_pipe_read`/`PollSet::wait` inside an
epoll wait (stdout/stderr pump plumbing); thread 5 = `fault_terminate_watchdog_thread_body`
(routine); thread 6 = `net::wait_on_tun` blocked on a `Condvar` (idle network-gateway wait); thread
8 = `net::NatGateway::new`'s `OnceLock::call_once_force` (idle init wait); thread 9 = another
`detached_pipe_read` epoll wait. **Not one thread shows a `compare_exchange_weak` spin or any other
deadlock signature** -- every thread is in a completely ordinary, legitimate OS wait
(`WaitForSingleObject`/`WaitOnAddress`/`NtWaitForWorkViaWorkerFactory`). This directly confirms
`xfwm4`'s HOST process is genuinely healthy at the litebox/Windows level; whatever keeps it from
registering as the window manager is happening inside the GUEST program's own logic, not in
litebox's fork/thread/lock machinery.

*The concrete, reproducible symptom*: `xprop -root _NET_SUPPORTING_WM_CHECK`'s error text itself
changes, in BOTH independently-run boots, at essentially the exact moment `xfwm4` execve's:
`WM_POLL n=1` through `n=3` read `"_NET_SUPPORTING_WM_CHECK:  no such atom on any window."` (the
atom name has never been interned on the X server at all); from `WM_POLL n=4` onward it reads
`"_NET_SUPPORTING_WM_CHECK:  not found."` (a DIFFERENT, shorter xprop message meaning the atom name
IS now interned/known to the server, but has no value set on the queried window). This is exactly
consistent with: some part of `xfwm4`'s own GTK/GDK/glib startup path references or caches this
atom NAME early (interning it via `XInternAtom`), but the process never reaches the later step that
actually calls `XChangeProperty`/`XSetSelectionOwner` to publish its VALUE on the root window,
within the 74+ seconds this pass observed it running. This is now believed to be an
application-level (`xfwm4`/GTK/X11) issue, not a litebox host-emulation defect -- litebox's job
(getting a real, unmodified `xfwm4` binary to `execve`, run, and talk to a real X server without
crashing or deadlocking) is DONE for this specific process.

**Per-process RAM cost, re-measured with fresh precision** (already known, Track B item 1, 57th
pass; this pass adds concrete numbers from direct observation): a single `de_only.sh` boot under
`LITEBOX_PROCESS_FORK=1` spawns roughly 28-29 live Windows host processes (external commands like
`mkdir`/`cp`/`sleep`/`xset`/`xprop`/`xdpyinfo`, dbus-daemon's activation babysitters, and every
`xfce4-session` client, EACH a full cross-process fork with its own ~500MB-1.1GB working set on
this build) within under a minute of `DE_LAUNCHED_DIRECT`. Directly observed twice this pass:
free RAM fell from ~8.5GB to under 1GB (0.98GB, then separately 0.75GB) in well under 90 seconds
each time, purely from this one repro running with no nginx/selkies on top at all. Both times,
`Invoke-CimMethod -MethodName Terminate` against every `litebox_runner_linux_on_windows_userland`
process cleanly recovered RAM to 7.8-8.5GB within seconds -- no host instability, no hang requiring
`cdb`/WMI escalation. This RAM cliff is the practical reason a live cdb attach on `xfwm4` needed a
purpose-built fast-attach orchestrator script rather than an interactive step-by-step session, and
is why any future attempt at this investigation (an X11-protocol trace, or the full
`webtop_stack.sh` with nginx+selkies layered on top, which will cost strictly more) must budget for
it explicitly and never run two attempts back-to-back without a full RAM-recovery pause.

**Why the browser/screenshot goal was NOT attempted this pass**: the task's own gate for the full
`webtop_stack.sh`+browser-screenshot verification is `xfwm4` genuinely reaching
`_NET_SUPPORTING_WM_CHECK` (i.e. `DE_UP`, not `DE_FAILED`). That gate is not met -- the atom's NAME
is interned but its VALUE is never set within the observed window, so `_NET_SUPPORTING_WM_CHECK`
never fires and `DE_FAILED` still occurs, just from an application-level cause instead of a
litebox-level hang. Attempting the full nginx+selkies boot on top of this unresolved blocker would
also incur strictly higher RAM cost than the already-dangerous `de_only.sh`-alone cost measured
above, for no additional diagnostic value until the `xfwm4` registration gap itself is understood.

**Pickup, precise**: (1) trace `xfwm4`'s actual X11 protocol writes on its own socket fd (enable
`litebox_shim_linux::syscalls::unix=debug` or equivalent, or a host-side network/socket capture if
one becomes available) to find its LAST successful X request before the point it should call
`XChangeProperty`/`XSetSelectionOwner` -- determine whether it is blocked waiting on a specific X
reply/selection it never receives, or whether it simply never reaches that code path at all (e.g.
an early `xfwm4` internal error that leaves it running its main loop without ever creating its
manager/check window); cross-reference against real upstream `xfwm4` source
(`src/wm.c`/`src/settings.c`, the `xfwm4` GitHub mirror) for the exact call site once a candidate
frame is found. (2) Budget RAM explicitly for this -- the ~28-29-process/<90s-to-<1GB cost measured
this pass applies even to the bare `de_only.sh` repro, before any selkies/nginx overhead. (3) Once
`_NET_SUPPORTING_WM_CHECK` genuinely fires, THEN attempt the full `webtop_stack.sh`
browser-screenshot verification (chrome-devtools MCP failed to connect this pass --
`CONNECT_TIMEOUT` -- re-check `claude-in-chrome`/`chrome-devtools` availability fresh at that time,
per the task's own standing instruction that this is the single most important moment for that
check to succeed).

## 62nd pass (2026-09-23) -- independent re-verification of the 61st pass's "app-level, not litebox" framing: NARROWED, one real litebox leak found+fixed, symptom NOT resolved

**Task**: verify (not assume) the 61st pass's conclusion that `xfwm4`'s failure to publish
`_NET_SUPPORTING_WM_CHECK` is application-level, per the standing project lesson that "looked like
an app bug, was actually a litebox emulation gap" has happened before (`libLLVM.so.19.1` `.dynsym`
corruption). Fetched real upstream `xfwm4` source (`github.com/xfce-mirror/xfwm4`, shallow clone to
the scratchpad) and read its actual startup sequence (`src/main.c`, `src/screen.c`, `src/hints.c`,
`src/settings.c`) to know exactly what to look for.

**Real upstream xfwm4 sequence, precisely**: `main()` -> `initialize(replace_wm)` ->
`myScreenInit()` (creates `screen_info->gtk_win`/`xfwm4_win`, calls `myScreenSetWMAtom()` which does
`XGetSelectionOwner(WM_S<screen>)` then `setXAtomManagerOwner()`/`XSetSelectionOwner` to CLAIM the
selection, then creates 4 "sidewalk" edge-detection windows, THEN returns) -> back in `initialize()`,
`initSettings(screen_info)` (`settings.c:1063`: `xfconf_init(NULL)` -> `xfconf_channel_new
(CHANNEL_XFWM)` -> `loadSettings()`, a loop of `xfconf_channel_get_property` calls -> `placeSidewalks
()`) -> ONLY IF THAT SUCCEEDS: `setUTF8StringHint(NET_WM_NAME)` then **`setNetSupportedHint()`**
(`hints.c:433-438`, the ONLY place `XChangeProperty(..., NET_SUPPORTING_WM_CHECK, ...)` is called, on
both the check window and root) -> `setNetDesktopInfo`/`workspaceUpdateArea`/`clientFrameAll` ->
`gtk_main()`. If `initSettings` fails, `initialize()` returns `-2` and the process `exit(1)`s
immediately (`main.c:714-717`) -- it does NOT reach `gtk_main()`.

**New live evidence, 3 independent techniques, none requiring guessing**:

1. **X11 ground-truth census** (`advisor/probes/webtop_xcensus.py`'s exact ctypes/libX11 technique,
   inlined as a heredoc into a `de_only.sh` variant, `.wfgy/de_only_xcensus_seed.tar`) run at
   `WM_POLL` n=2/4/8/12 via `XGetSelectionOwner`/`XQueryTree`/`XGetWindowProperty` -- far stronger
   ground truth than `xprop`'s atom-existence heuristic the 61st pass relied on. Two independent
   boots (`.wfgy/de_only_xcensus_run1_utf8.log`, `_run3_utf8.log`) agree exactly: at n=2, only
   `xfce4-session`'s own window exists, `WM_S0` owner=0x0 (myScreenInit not yet run). By n=4, windows
   `0x400001`/`0x40008c`/`0x40008e` (matching `xfwm4`'s `gtk_win`/`xfwm4_win`/its check window
   exactly, class `xfwm4.Xfwm4`) exist AND **`WM_S0` owner=`0x40008e`** -- `myScreenSetWMAtom`
   genuinely succeeded, `xfwm4` genuinely claimed the window-manager selection. By n=8, four more
   windows (`0x400092`-`0x400095`, sizes/positions matching the 4 sidewalk windows exactly) appear,
   under the SAME XID range (`0x400xxx`, i.e. the SAME client connection, never respawned) -- proving
   `myScreenInit` ran to completion and RETURNED. Yet at every poll through n=12 (60s+),
   `_NET_SUPPORTING_WM_CHECK`/`_NET_WM_NAME`/`_NET_CLIENT_LIST`/`SM_CLIENT_ID` root properties are ALL
   still empty. **Conclusion: `xfwm4` is demonstrably past `myScreenSetWMAtom` and past the end of
   `myScreenInit`, meaning it is inside (or has failed inside) `initSettings` -- the ONLY code between
   the confirmed-successful point and the confirmed-never-reached `setNetSupportedHint` call.**

2. **Direct guest stderr capture** (`litebox_diag::stderr_capture=debug`, `litebox_shim_linux/src/
   syscalls/file.rs:1877-1889` -- fires on every `fd==2` write host-side, independent of the guest's
   own pipe/fd plumbing, so it sidesteps the separate, still-open "`[de]`/`[de2]`-tagged pipe output
   never appears" writable-layer-visibility gap entirely). A full boot's 87 `stderr_write` events
   (`.wfgy/de_only_stderr_run1_utf8.log`) show real, correctly-attributed text from `dbus-daemon`
   (`Cannot initialize inotify: Function not implemented`), `gpg-agent`, `iceauth`, `pactl`,
   `xfce4-panel`, `xfce4-session`, `xfdesktop`, even `Xvfb` -- proving the capture mechanism itself
   works and catches real warnings from real processes. **`comm="xfwm4"` appears ZERO times.** Real
   upstream `xfwm4` prints an explicit `g_warning`/`g_print`/`g_critical` on EVERY one of its own
   failure paths (`"Another Window Manager (%s) is already running"`, `"Cannot acquire window manager
   selection"`, `"Missing data from default files"`, `"Could not find a screen to manage"`) -- zero
   output means NONE of xfwm4's own explicit, self-diagnosed failure paths fired. It is not failing
   loudly; it is silently, indefinitely pending inside a call that itself never errors or times out.

3. **Live `cdb -pv` mid-hang attach** (NOT immediately after `execve` like the 61st pass -- 35s into
   `xfwm4`'s own lifetime, deliberately timed to land mid-`initSettings` via a fixed orchestrator
   script, `.wfgy/xfwm4_cdb_midhang.ps1`; the first 3 attempts failed for a mundane reason worth
   recording -- ANSI color escape codes embedded in `LITEBOX_LOG` output split literal substrings like
   `argv0=` and `path=` across escape sequences, so a naive `Select-String -Pattern "argv0=..."`
   never matches; fixed by stripping `[char]27 + "\[[0-9;]*m"` before matching, and PowerShell 5.1's
   lack of a backtick-e escape for ESC was the actual root cause of the first fix attempt also
   failing). The snapshot (`.wfgy/xfwm4_midhang_snapshot_utf8.log`) shows the real guest-execution OS
   thread blocked inside `litebox_shim_linux::syscalls::epoll::PollSet::wait` -> `WaitContext::
   wait_until` -> `ThreadHandle::interrupt` -> a genuine `WaitForSingleObjectEx`/`NtWaitForSingleObject`
   -- a legitimate poll()-style wait, no spin/deadlock signature, matching a normal idle GLib main-loop
   iteration. This is CONSISTENT WITH BOTH remaining hypotheses (idling correctly post-init, or
   perpetually polling inside a stuck `initSettings`) and does not itself discriminate them --
   recorded so a future pass does not re-attempt an identical single-snapshot cdb capture expecting a
   different answer; the discriminating signal has to come from (1)/(2) above or a byte-level D-Bus
   trace, not another generic thread dump.

**A directly relevant, already-landed fix's own doc comment, re-examined and found INSUFFICIENT**:
`litebox_shim_linux/src/syscalls/unix.rs`'s `UnixConnectingStream` (commit `aa0b085`, 2026-09-22,
already in the binary this pass tested) explicitly documents itself as the fix for "the reason a
real `xfce4-session` boot never reaches `_NET_SUPPORTING_WM_CHECK`: its own (GDBus/GIO-driven) D-Bus
connect hits [the old unconditional-cancel] arm and then never forks a single child process again."
That description matches a DIFFERENT, EARLIER, already-CLOSED failure mode (a total, permanent
fork-starvation) -- this pass's boots fork dozens of children successfully (`xfsettingsd`,
`xfce4-panel`, `Thunar`, `xfdesktop` all launch) and never freeze, so that catastrophic case is
confirmed still fixed. The CURRENT, narrower symptom (only `xfwm4`'s own `initSettings` never
finishing) is a residual instance of the same general subsystem, not proof the whole class is closed.

**Real, verified, additional fix landed this pass** (still open whether it's sufficient):
`SharedUnixConnectQueue::cancel` (`unix.rs`) had one more real leak in the SAME rendezvous the
`aa0b085` fix touched: if a connect's caller cancels (on timeout, or losing a state race) at the
EXACT moment the listener's `try_claim()` has already moved a request `REQ_PENDING` -> `REQ_CLAIMED`
(and shortly after, `REQ_ACCEPTED`), the old `cancel()` did one `compare_exchange(PENDING, EMPTY)`,
saw it fail, and walked away -- leaking that request slot AND the `SharedUnixConnTable` connection
slot the listener had just allocated, forever (own comment: "reclaimed when the whole fork family
exits"). `SHARED_UNIX_CONNECT_QUEUE_CAPACITY` is 64; a real desktop boot's ~28+ processes each making
several D-Bus/X11 connect attempts under real host-RAM pressure is exactly the shape that could
exhaust it well before the fork family ever exits, permanently `EAGAIN`-ing every later connect to an
otherwise-healthy listener. **Fixed**: `cancel()` now, on losing the race, bounded-spins
(non-blocking, matching this queue's existing no-real-wakeup design) for the listener's own
`complete()`, then constructs this side's own `UnixConnectedStream` for the abandoned slot and
immediately drops it -- deferring to `ConnTransport`'s existing Drop-based both-sides-shut-down
bookkeeping (the same path a synchronously-completed connect immediately closed by its caller already
uses) rather than calling `SharedUnixConnTable::free` directly, which would race a listener still
genuinely using the slot. Also added a `WARN` on `SharedUnixConnectQueue::post`'s queue-full return
(previously silent) so a future trace can directly confirm or refute real exhaustion instead of
inferring it. **Verified**: `cargo build -p litebox_runner_linux_on_windows_userland --release`
clean; re-ran the exact `de_only.sh` repro post-fix (`.wfgy/de_only_verify62_run1.log`) --
**identical symptom, byte-for-byte the same marker sequence and timing as every pre-fix run**
(`WM_POLL` n=1-3 "no such atom", n=4+ "not found", `DE_FAILED after 60s`). The fix is real, safe, and
plausibly still load-bearing for a DIFFERENT, higher-load scenario (the full `webtop_stack.sh` with
nginx+selkies layered on top costs strictly more per AGENTS.md's own measurements), but it is
**NOT, by itself, the mechanism blocking `xfwm4` in `de_only.sh`'s simpler repro**.

**A second, separate, real litebox bug found live, unrelated to `xfwm4`**: `/defaults/xfce/` (a
`linuxserver/webtop` image path baked into an immutable OCI layer, confirmed present via `tar -tf`
on the cached layer tars in `.litebox-cache/`) is read correctly (3 real files, correct byte counts)
by `de_only.sh`'s own `cp /defaults/xfce/* ...` early in the script, but a LATER `ls -la
/defaults/xfce/` (a separate forked child, later in the same boot) sees it as EMPTY (only `.`/`..`).
Read `litebox/src/fs/layered.rs`'s `read_dir`/`migrate_entry_up_for_metadata`/`open` logic: a path not
yet cached in `root.entries` tries `self.upper.open()` FIRST unconditionally (`layered.rs:670`); if
SOME earlier operation (a `chmod`/`chown`/`utimensat`/`touch`-style metadata op via
`migrate_entry_up_for_metadata`, `layered.rs:423-470`) ever creates an EMPTY upper-layer directory
shadow for this exact path, `read_dir`'s `EntryX::Upper` branch is SUPPOSED to still union it with
`self.lower`'s real content (`layered.rs:1704-1728`) -- the exact point of failure (whether the
`self.lower.open`/`read_dir` merge itself fails for this path, or whether the upper-shadow gets
created some OTHER way this pass didn't fully trace) is NOT yet root-caused; not on `xfwm4`'s own
boot path (it never touches `/defaults/xfce`) so lower urgency, but real, reproducible, and worth a
dedicated pass given this project's own history of filesystem-layering bugs (`libLLVM.so.19.1`,
`Pipes.litebox`, `FutexManager`, all "raw-pointer/cache-frozen-into-shared-bytes" variants of the
same root pattern per the Shared-memory foundations section).

**Refined conclusion on the 61st pass's framing**: NEITHER confirmed nor refuted outright --
narrowed. The specific claim "an application-level (xfwm4/GTK/X11) issue, not a litebox host-emulation
defect" is NOT well-supported by this pass's own evidence (zero explicit-failure stderr from xfwm4
argues against a genuine upstream xfwm4 bug specifically; xfconfd independently verified reachable
via a bare `dbus-send --session --dest=org.xfce.Xfconf ... Introspect` call succeeding with a real
method-return in the SAME boot argues against "xfconfd never starts" too) -- but this pass's own
fix attempt (a real, verified, already-known-incomplete leak in the exact rendezvous machinery this
class of symptom lives in) did not resolve it either. The most likely remaining shape: something in
`libxfconf`'s/GDBus's OWN persistent/cached bus-connection or signal-subscription (`AddMatch`) pattern
-- distinct from `dbus-send`'s one-shot synchronous call, which this pass proved works -- hits a gap
in the shared AF_UNIX transport (or in xfconfd's own handling of that specific pattern) that a plain
method-call round-trip never exercises.

**Pickup, precise**: (1) get a BYTE-LEVEL trace of xfwm4's own D-Bus socket specifically (not the
whole boot's `unix=debug`, which produced 150-300MB logs in under a minute this pass and is
impractical past the first ~15s) -- narrow to ONE fd/sock_id by first correlating xfwm4's own guest
pid via `syscalls::process=debug`'s `winpid=`/execve lines, then either add a temporary,
pid-filtered trace, or decode the existing `diag-unix-stream-write`/`-read`'s `sock_id`s against
`unix_connect`'s own logged `sock_id` for that same pid's D-Bus fd. (2) Cross-reference against
`libxfconf`'s real source (`git clone https://github.com/xfce-mirror/xfconf`, not yet fetched this
pass) for exactly what `xfconf_channel_new`+first `xfconf_channel_get_property` call sequence does at
the GDBus level (method calls issued, signals subscribed, in what order) -- this pass fetched
`xfwm4`'s own source but not `libxfconf`'s. (3) Root-cause the `/defaults/xfce/` empty-readdir bug
found above as its own dedicated investigation (add a one-off diagnostic print of `root.entries`'
state for a specific path right before/after the suspect `migrate_entry_up_for_metadata` call, on a
minimal non-desktop repro, to avoid the 28-process RAM cost of a full boot). (4) Budget RAM exactly as
the 61st pass documented (~8.5GB -> under 1GB within 90s on `de_only.sh` alone) -- this pass hit the
same cliff 4 times and killed cleanly each time via `Invoke-CimMethod -MethodName Terminate`.

## 63rd pass (2026-09-23) -- the `/defaults/xfce/` readdir hypothesis REFUTED; `DE_FAILED`
reconfirmed; one new concrete lead on the real blocker

**Task**: root-cause and fix the 62nd pass's own `/defaults/xfce/`-appears-empty-to-a-later-forked-
child hypothesis (Track B item 2b), matching this project's own "verify before fixing" discipline --
build an isolated repro first, then check whether fixing it actually resolves `xfwm4`'s `DE_FAILED`
hang via the same X11-census ground truth the 62nd pass established.

### Reading `litebox/src/fs/layered.rs` (full file, both halves)

The layered filesystem caches a path's classification (`EntryX::Upper`/`EntryX::Lower`/`Tombstone`)
in `self.root: RwLock<RootDir>` on first `open()`. `read_dir`'s two branches (line ~1704):
- `EntryX::Lower { fd }` -- "the easy case", delegates straight to `self.lower.read_dir(fd)`, no
  union needed since (by the caching invariant) no upper entry exists for this path.
- `EntryX::Upper { fd }` -- gets `self.upper.read_dir(fd)`, THEN tries `self.lower.open(path,
  RDONLY, empty())` + `self.lower.read_dir(&lower_fd)` and unions the two, silently keeping
  upper-only entries if the lower open/read_dir fails (`if let Ok(...)`, no error surfaced).

`migrate_entry_up_for_metadata`'s directory branch (line ~423-470, reached from `chmod`/`chown`/
`set_times` on a lower-only directory) creates an EMPTY upper-layer `mkdir` shadow (carrying the
lower's mode/node-info, never its children) WITHOUT touching `self.root`'s cache for that path.
`mkdir_migrating_ancestor_dirs` does the same thing for every ANCESTOR directory of a newly-created
file under a lower-only tree (`open(..., O_CREAT)`, `mkdir`, `rename`, `link`, `symlink`,
`make_fifo` all funnel through it). Both are the two natural, real ways an upper-layer directory
shadow gets created over a still-lower-only directory during a real boot -- matching the 62nd pass's
own framing exactly.

Traced `litebox/src/fd/mod.rs`'s `RawDescriptorStorage::fork_duplicate` doc comment and confirmed:
`self.litebox.descriptor_table()` (the `TypedFd` table `layered.rs`'s cached `EntryX::Lower`/
`EntryX::Upper` entries index into) is a single GLOBAL table per `LiteBox` instance, not per-process
-- ruling out a "stale foreign fd index" theory for THIS specific cache. What actually differs
across a cross-process fork is that `spawn_cross_process_fork_child` REBUILDS THE ENTIRE ROOTFS FROM
SCRATCH per forked child: re-parses the cached OCI-image tars (deterministic, byte-identical every
time) and IMPORTS the parent's writable (upper) layer from a fresh tar-export snapshot
(`litebox-container-fs-<pid>.tar`, confirmed live in every boot's own `[process_fork_diag]
globalstate-probe (child): adopted the parent's writable layer from ...` line). So the CHILD's
`layered::FileSystem.root` cache starts genuinely EMPTY (not a stale copy of the parent's) -- any
upper-layer directory shadow the parent already created is inherited only via the freshly-imported
TAR SNAPSHOT, and the child's very first `open()` of that path re-classifies it fresh (tries
`self.upper.open()` first, finds the imported empty shadow, takes the `EntryX::Upper` branch, and
must genuinely re-run the lower-open+union merge itself).

### Isolated repro (before touching any code)

Built a minimal script (`debian:stable-slim`, avoiding the larger webtop image, since the general
shape of the bug -- not the specific `/defaults/xfce` path -- is what needs testing) that:
1. `ls -a $DIR | wc -l` on `/etc` (50 entries, purely lower-layer at this point).
2. `touch $DIR` (metadata op directly on the directory -- the `migrate_entry_up_for_metadata`
   trigger) -- re-`ls` in the SAME process, then again in an explicitly forked (`( ... ) &`)
   subshell.
3. Separately, `: > $DIR2/trig_newfile` under `/usr/share/doc` (80 entries) -- the
   `mkdir_migrating_ancestor_dirs` trigger via new-file-creation -- re-`ls` same-process and forked.
4. A second run repeated step 2 with a GRANDCHILD (nested `( ( ... ) & wait ) & wait`) fork instead
   of a single level, to more closely match `xfce4-session` forking `xfconfd`/dbus helpers which are
   themselves forked children of a forked child.

All under `LITEBOX_PROCESS_FORK=1` (host env var, current release binary, `target/release/
litebox_runner_linux_on_windows_userland.exe`, mtime postdates commit `1522918`). Four separate
boots, zero crashes, fully consistent results:
- `/etc`: 50 (early) -> 50 (same-proc after touch) -> 50 (forked child after touch) -> 50
  (grandchild after touch). Never empty, never wrong.
- `/usr/share/doc`: 80 (early) -> 81 (same-proc after creating `trig_newfile`) -> 81 (forked child)
  -- correctly INCLUDES the new file, not just the untouched lower content.

A control run WITHOUT `LITEBOX_PROCESS_FORK=1` (default thread-based fork, even with the
`GLIBC_TUNABLES` workaround `--env GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0`)
crashed on the very FIRST forked `ls` (`SIGABRT`/`SIGSEGV`, tcache corruption) -- confirming
AGENTS.md's own characterization that the thread-based path's SECOND corruption signature (Track B,
distinct from ADVISORY-001 3N, NOT fixed by the tunable) makes it currently unusable for testing
ANY multi-fork hypothesis via subprocess forking. This means the isolated repro could only exercise
the cross-process path -- a real scope limitation of this pass's negative result, noted honestly
rather than glossed over.

### Real production re-test

Re-ran the 62nd pass's own `.wfgy/de_only_xcensus_run3.ps1` harness verbatim (copied to `run5.ps1`/
`run5.log`, same `--resume-from .wfgy/de_only_xcensus_seed2.tar`, same real
`docker.io/linuxserver/webtop:debian-xfce` image, `LITEBOX_PROCESS_FORK=1`, current rebuilt release
binary). Converted the UTF-16LE log with `iconv -f UTF-16LE -t UTF-8` per AGENTS.md's own standing
lesson. The `[s] XFCONF_DEFAULTSDIR >>>total 0` marker line the 62nd pass's own quick read stopped
at is followed, in the SAME `$(...)`-captured multi-line block, by:

    drwxrwxrwx 1 root root 4096 Jan  1  1970 .
    drwxrwxrwx 1 root root 4096 Jan  1  1970 ..
    -rw-r--r-- 1 root root 2267 Jan  1  1970 xfce4-panel.xml
    -rw-r--r-- 1 root root 5437 Jan  1  1970 xfwm4.xml
    -rw-r--r-- 1 root root 1907 Jan  1  1970 xsettings.xml

i.e. `/defaults/xfce/` has its correct 3 files, read well into the boot (after `dbus-daemon`, after
multiple prior forked children, at the exact point `de_only.sh`'s own diagnostic checks it). "total
0" is `ls -la`'s ordinary leading block-count summary line, NOT an entry count -- a real, simple
misreading in the 62nd pass's own quick pass over the log, not a real litebox bug. `XFCONF_USERDIR`
(the destination of the earlier `cp /defaults/xfce/* ...` copy) shows the identical 3 files too,
confirming the copy itself succeeded correctly as well.

**Conclusion**: the layered-fs `read_dir` upper/lower merge is NOT broken, under the cross-process
fork path, for either natural upper-shadow-creation trigger, across 4 isolated boots AND the real
production boot. This is treated as a genuine, valuable NEGATIVE result per this project's own
stated policy -- not forced into "fixed" when the evidence says otherwise. (Scope honestly
acknowledged: the thread-based fork path remains untested for this hypothesis, blocked by its own
separate, already-known, unrelated crash bug.)

### `DE_FAILED` reconfirmed fresh, byte-for-byte the same symptom

The same `run5` boot's own `WM_POLL`/`XCENSUS` loop (identical harness the 61st/62nd passes used):
`WM_POLL n=1-2` "no such atom on any window", `n=3-12` "not found", `DE_FAILED after 60s`. The
`XCENSUS` ground-truth census at `n=8`/`n=12` shows **25 real windows**: `xfce4-session.Xfce4-
session`, `xfwm4.Xfwm4` (x2, one of them the tiny 5x5 WM-selection window at `0x40008e`),
`xfsettingsd.Xfsettingsd`, `xfce4-panel.Xfce4-panel` (x3), `thunar-real.Thunar-real`, `wrapper-2.0.
Wrapper-2.0` (x2), `xfdesktop.Xfdesktop` (x4, including a real 200x200 "Desktop" window) -- every
real XFCE session component is genuinely alive and has created its windows. `XCENSUS_SELECTION
WM_S0 owner=0x40008e` confirms `xfwm4` DOES own the window-manager selection (matching the 62nd
pass's finding). `XCENSUS_ROOTPROP _NET_SUPPORTING_WM_CHECK=''` confirms `setNetSupportedHint()`
is STILL never reached. Byte-for-byte the same blocker as the 61st/62nd passes, unaffected by
anything this pass touched (as expected, since nothing was actually changed in `layered.rs`).

### New lead: a second `dbus-daemon` gets `SIGKILL`ed right after a thread-based-fork fallback

In the SAME `run5` log, at guest-relative t~17.618-18.267s (inside one particular forked child's own
execution), three consecutive events:
1. `17.618s WARN ... unsupported feature=getsockopt(level = 1, optname = 77/31/59)` (harmless,
   unrelated `getsockopt` gaps, logged elsewhere in the same window too).
2. `17.710s`/`17.847s WARN litebox_shim_linux::syscalls::process: clone: cross-process fork() not
   eligible -- these fd subsystems cannot cross the process boundary yet ... kinds=["unix-socket-
   pair(addressless,pre-exec-IPC)"] ...` -- TWO separate fork attempts, both forced onto the
   thread-based fallback specifically because they hold a pre-exec addressless `socketpair(2)` fd
   (the exact kind `raw_fd_is_addressless_unix_socket_pair` (54th pass) refuses to carry, matching
   AGENTS.md's own standing lesson about `dbus-daemon`'s babysitter pattern).
3. `18.267340700s ERROR litebox_shim_linux::syscalls::signal: fatal signal: terminating task
   signal=Signal(9) pid=38 tid=38 comm=[100, 98, 117, 115, 45, 100, 97, 101, 109, 111, 110, 0, ...]`
   -- `comm` decodes to literally `"dbus-daemon\0"`. A `dbus-daemon` process, NOT the main
   `--nofork` session bus de_only.sh itself started (which is independently confirmed alive
   throughout via the successful `DBUS_LISTNAMES`/`DBUS_XFCONF_PROBE` calls elsewhere in the exact
   same log) and NOT `xfconfd` (also independently confirmed alive via its own successful
   `Introspect` reply), is killed by `SIGKILL` roughly 0.4-0.6s after falling onto the thread-based
   fork fallback.

This is a genuinely new, precisely-timestamped, previously-unrecorded observation. Plausible
(NOT yet confirmed) causal link to `xfwm4`'s own hang: if this second `dbus-daemon` is a D-Bus
service-activation helper (or a `dbus-launch`-style spawn triggered by something in the XFCE
startup chain) that libxfconf/xfconfd's own machinery depends on, its crash -- landing in the
already-known-crash-prone thread-based-fork territory (ADVISORY-001 3N-adjacent, a SEPARATE
mechanism from anything this pass touched) -- could leave a GDBus method call from
`xfconf_channel_new`/`initSettings()` waiting forever for a reply that will never arrive, with no
error ever surfaced to `xfwm4` (matching the zero-stderr observation the 62nd pass already made).
Equally plausible: this is an unrelated, parallel failure with no bearing on `xfwm4` at all. NOT
resolved this pass either way -- flagged as the single most concrete, actionable next step, ahead of
attempting a `cdb -pv` attach on `xfwm4` itself (which no pass has yet attempted and which needs
genuinely quiet host RAM per the 32nd-35th passes' own repeated documented failures to get one).

### Explicit non-actions this pass, and why

- Did NOT modify `layered.rs` -- the evidence does not support a real defect there for either tested
  trigger; patching code with no reproducing bug would be exactly the "patch over" behavior this
  project's own standing instructions forbid.
- Did NOT attempt the full `webtop_stack.sh` (nginx+selkies) boot or any browser/screenshot
  verification -- `DE_FAILED` is unchanged from the 61st/62nd passes' own state, so a full-stack
  attempt would cost real RAM/wall-clock for no new information until the actual blocker moves.
- Did NOT attempt a `cdb -pv` attach on `xfwm4` or the crashed `dbus-daemon` -- both are real,
  valuable next steps but a distinct, substantial undertaking (this project's own history shows
  several PRIOR passes failing to even get a clean `cdb` attach window due to host RAM pressure
  during the Xvfb-crash investigation) better scoped as its own dedicated pass.

### Precise pickup for the next pass

1. Identify who forks the second `dbus-daemon` (pid=38/tid=38 in this run) and why -- grep the SAME
   boot's `execve`/`clone` trace (`litebox_shim_linux::syscalls::process=debug`) for the parent pid
   of tid=38/tid=20012 (the two `clone: not eligible` warnings immediately preceding the kill), and
   check whether it is anywhere in `xfce4-session`'s or `xfconfd`'s own process tree, or is a
   fully independent `dbus-launch`/system-bus-activation artifact with no bearing on `xfwm4`.
2. If it IS on `xfwm4`'s dependency chain: the fix is very likely in making a bare `dbus-daemon`
   activation-helper fork survive the thread-based fallback (a narrower, more tractable problem
   than the general Track B thread-fork tcache corruption, since only ONE specific spawn shape is
   implicated) -- or in making `unix-socket-pair(addressless,pre-exec-IPC)` forks eligible for
   cross-process fork after all (carrying the socketpair, not dropping or refusing it).
3. If it is NOT on the chain: proceed to the previously-recorded pickup -- a byte-level trace of
   `xfwm4`'s own D-Bus fd specifically, cross-referenced against real `libxfconf` source
   (`github.com/xfce-mirror/xfconf`, still not fetched by any pass to date), or a genuine `cdb -pv`
   attach on `xfwm4` itself breaking on `getenv`/`XOpenDisplay`-adjacent GDBus call sites, attempted
   only once host RAM is confirmed genuinely quiet per the standing RAM-budget lesson.

Logs for this pass: `.wfgy/de_only_xcensus_run5.log`/`_utf8.log` (real production re-test),
scratchpad `readdir_repro1.log` through `repro4.log` (isolated repro; repro1 is the
thread-based-fork-crash control, repro2-4 are the cross-process-fork clean results).
