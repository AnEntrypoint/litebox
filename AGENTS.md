# litebox — current state (2026-09-21)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, and a claim a later commit superseded, were deleted rather than hedged. Reference
detail is drained to the `docs/AGENTS_ARCHIVE_*.md` files and per-investigation logs to the dated
`docs/*.md` in the map below — read those for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one
line plus its pointer, not in a separate memory file.

## The cheap repro — start here

```
target/release/litebox_runner_linux_on_windows_userland.exe -Z \
  --oci-image docker.io/library/debian:stable-slim -- /bin/bash -c '<script>'
```

One ~81MB layer, `[cache] HIT` after the first pull, real GNU coreutils instead of busybox — which
matters: coreutils `touch` issues the `utimensat(fd, NULL, …)`/futimens form busybox's never reaches
(`caaac79`). Two host-side gotchas, each already costly:

- **PowerShell, never Git Bash** — Git Bash rewrites `/absolute/guest/paths` into
  `C:/Program Files/Git/...` before the runner sees them, giving a misleading `ENOENT`. `Start-Process
  -RedirectStandardOutput/-RedirectStandardError` makes the runner exit almost instantly with zero guest
  output (no crash dump, no event-log entry); use `& .\runner.exe ... *> combined.log` instead.
- **Single quotes only inside `-c`** — embedded double quotes are corrupted crossing into the child's
  Win32 command line. This masqueraded as deep fork/stack-pointer corruption for a whole sub-session.

**Log level**: default is `warn,litebox_platform_windows_userland::fork_verify=error` (`EnvFilter`'s
own ERROR-only default discarded all real `warn!` sites; `fork_verify` is pinned to `error` because
it warns per single-stepped instruction). Do **not** add `LITEBOX_LOG=error` by reflex; use
`fork_verify=warn` when a fork heal is the subject.

## Standing lessons and hard constraints

- **No WSL or hypervisor, ever** — always run under the matching runner
  (`litebox_runner_linux_on_windows_userland.exe`/`litebox_runner_linux_userland`); cross-compiling FOR
  Linux is fine, running the result in a VM defeats the premise.
- **`fork_verify.rs`'s stale-pointer-healing bug class is Windows-only** (real `fork()` gives the
  child identical addresses) — never port to another platform's crate.
- **Never `bcdedit /debug on`** without a kernel debugger already attached — two full-host freezes
  needing a power-cycle.
- **A process spinning inside a dead-locked allocator/spinlock resists `Stop-Process -Force`** —
  use `Invoke-CimMethod -MethodName Terminate` (WMI) instead. `cdb -p <pid>` must use `-pv`/`qd`,
  never a bare `q` (kills the target).
- **Never run two full-stack verifications concurrently** — starves both, looks exactly like a real
  hang. Kill every `litebox_runner` between runs; watch `FreePhysicalMemory`, kill on a falling
  trend not a fixed RSS number.
- **`LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check**, never
  `PrintWindow`/`CopyFromScreen`. A pixel count alone never identifies WHO painted a frame — decode
  frame structure (`advisor/probes/decode_frame.py`) and correlate against `DIAG_TIMELINE execve`'s
  real argv0.
- **Never time litebox with one host process per datapoint** (bare spawn costs 1.6-2.3s) — run N
  iterations inside ONE guest process. Never subtract timestamps across a parent log and a
  fork-child log — `init_logging()` resets elapsed time to ~0 per child.
- **Release-binary `cdb` reads are unreliable** — MSVC linker ICF folds distinct functions into
  one symbol (no `[profile.release]` override exists, so LTO is off but ICF still runs). Build
  `cargo build -p litebox_runner_linux_on_windows_userland` (no `--release`) for any `cdb` session
  needing a trustworthy stack — confirmed live, eighteenth pass, refuted two release-build leads.
- **Refusal errno choice is API contract** — EPERM lets callers degrade, EINVAL/ENOSYS fails them
  hard; wrong choices have silently broken whole subsystems before (archive; 30th pass's AF_UNIX
  `EAGAIN`-vs-`EINPROGRESS` connect() fix is the newest instance).
- **Proving a run took the cross-process fork path needs `[process_fork_diag] task-resume-probe` lines,
  never the shim's eligibility log** — that log fires regardless of whether the fork actually happened
  that way (produced two recorded false conclusions, archive).
- **fork carries pipes, regular files and the writable layer into a child, but NOT sockets** — a
  pre-fork-created listening socket serves nothing to a forked child; run dbus-daemon non-forking, and
  for XFCE use `xfce4-session`, never `startxfce4`.
- **On host-side crashes, use `advisor/probes/symbolize_litebox_crash.py`, snapshotting `.exe`+`.pdb`
  next to the log** — a ring dump's `rva=` is only meaningful against the exact emitting build.
- **Isolate the harness before blaming litebox** — launch guest probes directly as the runner's
  top-level program, never via a runtime-built `/bin/sh -c` wrapper.
- **Never trust a container tag name for its WM/session contents** — verify by registry manifest +
  blob tar-listing, or a live in-guest `/usr/bin` listing.
- **Never record a test count not watched run to completion**; never leave a suite red for an
  environmental reason.
- **Repo hygiene** — packed layer tars, frame dumps and debug logs never go in git (`.wfgy/`,
  gitignored); untrack anything `git add -A` sweeps in.
- Procedural know-how is in the archive's "Working practices": freestanding guest binaries built on the
  HOST, probe injection via a small `--resume-from` overlay tar, mature libraries over hand-rolled code.
- **Guest-reachable code returns an errno, never a panic** — the host process IS the entire guest
  session, so an `unimplemented!()`/`unreachable!()`/panic, or unbounded recursion, on any
  guest-reachable path kills every guest process at once. Bitten many times (OOM, metadata ops, open
  flags, nested `epoll_ctl`, corrupted guest contexts); full fixed-bug list with shas: archive.

## Cross-process fork (`LITEBOX_PROCESS_FORK=1`)

A genuine `D == 0` fork — child at the SAME addresses, no relocation, no `fork_verify` healing — exists as
`spawn_cross_process_fork_child` (design case `advisor/ADVISORY-002-d-zero-fork.md`). It short-circuits to
a native fork when `platform.has_native_fork()` — the whole fd-carrying apparatus is Windows-only
scaffolding for a missing syscall.

**It is correctness-sound**: zero corruption across every completed fork on a `bash -c` loop repro, vs the
thread-based default's 100% tcache-corruption rate on the same repro (ADVISORY-001 §3N is the
**thread-based** path's defect only).

**Eligibility** — an already-borrowed fd table, a beyond-stdio fd that isn't a pipe end/path-recorded
regular file/eventfd/close-on-exec (overridable by `LITEBOX_PROCESS_FORK_IGNORE_FDS`), or an
unsanitizable `fs_base`/context. The old unconditional-by-`comm`-name refusal for `Xvfb`/
`dbus-daemon` is REMOVED as of the twelfth pass (below) — both now go through this same scan like
everything else. On a real `debian-xfce` boot the only remaining blocking kind is `unix-socket` — 5
refused forks of 34, down from 34/34 (pre-twelfth-pass baseline). Per-kind deviations: archive.

**Per-fork cost** was ~3.5-5s, now ~1.2s; `NGINX_STARTED` in under a minute versus never in 15+.
Use `LITEBOX_DIAG_FORK_TIMING=1` for the next cost question.

**Still open**: nginx's own SSL-cert generation fails on its first real startup attempt — the
original symptom this investigation began from, genuinely not root-caused
(`docs/track-b-fork-fix-progress.md:146-152`).

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
process. Full narrative, every repro command, every ruled-out alternative for all four passes:
archive.

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
`getenv()`/loader-stack all proven correct by direct evidence) — **current top blocker to the
browser/apps milestone, alongside the still-open Xvfb call site** — needs a live `cdb` attach on
`xfce4-session` itself (breaking on `getenv`/`XOpenDisplay`/`_XConnectXCB`), never yet attempted by
any pass, only once host RAM is genuinely quiet (6+ GB free, no heavy unrelated host load — three
33rd-pass attempts and one 34th-pass attempt at 1.6-4GB free all died pre-Xvfb to the SAME
already-documented thread-fork tcache corruption class before ever reaching `xfce4-session`).

**Thirty-fourth pass (2026-09-22) — fork-eligibility mechanism confirmed by static code reading; no
by-name gate exists, the only gate is a global opt-in env var + a per-fork fd-kind scan; flipping
that env var on for the real desktop boot is confirmed still NOT a safe narrow win (a real, separate,
possibly-stale "Fork-after-Xorg" freeze risk, never re-tested since); live re-verification blocked
again by the SAME pre-Xvfb host-load condition the 33rd pass hit.** Full evidence, exact line
numbers, and the precise pickup for both open threads: archive.

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
(that string recurs for totally unrelated root causes elsewhere in this project's own history — see
archive). Full evidence, exact log line numbers, fix mechanics: archive.

**Thirty-sixth pass (2026-09-22) — `pty_registry`/`daemon_pty_masters` cross-process redesign
(pickup item 4b's pty slice): landed and compile/unit-verified; live cross-process byte-transfer
NOT yet confirmed.** Root cause matched the SAME defect class already fixed nine times over
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
was not suffient alone. Added `syscalls::pty::SharedPtyTable<Platform>` (new, `litebox_shim_linux/
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
the id locally. A new `PtyStateRef` enum (`Local(&Arc<PtyPair>)` vs `Shared(id, &SharedPtyTable)`)
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

**Verification, precisely scoped**: (1) `cargo check -p litebox_runner_linux_on_windows_userland`
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
pass (host RAM ~2.7GB free throughout, but time-budgeted toward the design/implementation/local-
regression work instead); needs a follow-up pass with either a tiny custom freestanding guest
binary (matching this project's own "freestanding guest binaries built on the HOST" working
practice, avoiding `script`'s interactive-terminal assumption) driving `/dev/ptmx` +
`LITEBOX_PROCESS_FORK=1` fork + a child re-opening `/dev/pts/<id>` by number, or a `--pty-mode`
session-daemon repro exercising `pty_master_read`/`write`'s new fallback directly.

### Track B — current pickup list, precise (full pass-by-pass evidence: archive)

(-1) ~~Build the minimal isolated cross-process AF_UNIX repro~~ — DONE, fourteenth pass. (0)

(0b) `SafeZoneAllocator`'s `spin::mutex::SpinMutex` (`litebox/src/mm/allocator.rs`) needs the same
dead-holder-recovery treatment `RawMutex` already has — live-caught spinning forever in `dealloc`,
high blast radius, own dedicated pass. (2) debugger-root-cause `litebox/src/event/wait.rs:224`'s
`unreachable!()` on garbage thread state (dozens per boot, most frequent panic historically, NOT
yet debugger-confirmed — do not patch blind). **Current top blocker, thirty-second pass — hardware
ground truth in hand (bit-identical `rip`/fault-address across two independent boots, glibc AVX2
memcpy/memmove reading 64+ bytes from a fully-unmapped `0x7feffecdd400`), exact Xvfb call site
still open**: pickup is real CFI-based stack unwinding (no tool for this was readily available
this pass — `.wfgy/xvfb.debug`'s DWARF `.eh_frame`/`.debug_line` data is already fetched and
matches the exact running `BuildID sha1=6440f00c805782c9a39a5acd92855079e9fffc92`, a naive
raw-stack-word scan surfaced 4 plausible-but-probably-stale candidates) or upstream Xvfb source
cross-reference (network access confirmed working — `debuginfod.debian.net` answered in one
request) to determine whether this is a litebox-shim bug or a genuine upstream Xvfb defect. Use
`LITEBOX_DIAG_FATALDUMP=1` (no debugger — attaching one was proven this pass to starve the boot
of the traffic the crash needs), NOT cdb, for the next capture. **Also found and fixed this same
pass, unrelated regression**: `LITEBOX_DIAG_FATALDUMP=1` alone was ALSO firing `diag_raw_regdump`
on every `EXCEPTION_SINGLE_STEP` (routine, per-instruction, during `fork_verify`'s thread-based-
fork healing) — live-hit as a multi-minute full-stack-boot stall when one fork fell back to the
thread-based path and its own single-step healing loop never converged; fixed (`f1da614`,
skips single-step for the `diag_fataldump_enabled()`-alone path, `veh_trace_enabled()` unchanged).
**Release-binary full-stack attempt (`.wfgy/webtop_stack.sh`, `--publish 8081:8081`) this same
pass**: reached `XVFB_UP`/`DBUS_UP`/`SELKIES_PORT_UP`/`DE_LAUNCHED` cleanly post-fix — the fix
above is load-bearing for the release binary too, not just debug. Host RAM then fell to ~720MB
free right at that point (recovered to ~2.2GB moments later, fully recovered after kill) —
**never got a confirmed browser render this pass**: both `chrome-devtools` MCP (connection
timeout) and `claude-in-chrome` (extension not connected) were unavailable in this environment,
and a plain `curl http://localhost:8081/` timed out during the low-RAM window — inconclusive
whether that was the RAM pressure, the imminent Xvfb crash, or selkies itself. Also hit one
genuine ~9-hour HOST SLEEP mid-session (`LastBootUpTime` unchanged, so suspend/resume not a
reboot) that silently paused a boot attempt for hours — an environmental risk for any future
long-running session on this host, worth disabling sleep before the next attempt. **Pickup**: fix
(or ask the user to fix) browser-tool connectivity first, then repeat the release-binary boot
(now cheap and reliable past `DE_LAUNCHED`) and connect promptly after `SELKIES_PORT_UP`/
`DE_LAUNCHED` fire, before investigating the RAM spike further.
~~DISPLAY/getenv() as the DE_FAILED cause~~ — REFUTED FOR GOOD, thirtieth pass (see
that pass's own entry above for the LD_PRELOAD-interposer evidence). ~~AF_UNIX connect()
EAGAIN-vs-EINPROGRESS~~ — FIXED, thirtieth pass; that same code path's
request-cancellation-on-first-non-blocking-miss behavior is its own separate, precisely-scoped, NOT
yet fixed follow-up — see that pass's own entry above for the exact
`UnixStreamState::Connecting(request_idx)` reasoning. (4) finish the `Network` shared-arena redesign (`interface`
remains — `queued_for_closure` is fixed, twenty-eighth pass); ~~(4b) `pty_registry`/
`daemon_pty_masters`~~ — DONE, thirty-sixth pass (`syscalls::pty::SharedPtyTable`,
`litebox_shim_linux/src/syscalls/pty.rs`); compile+local-regression-verified, live cross-process
byte transfer still pending a `LITEBOX_PROCESS_FORK=1` repro (see that pass's own entry above).
`flock_registry`/`drm`/`evdev` (`GlobalState` fields, eighteenth-pass audit) remain open — same
non-POD-payload (Arc-based state, `Pollee` observer lists) obstacle the pty fix's own `PtyEnd::
Shared{Master,Slave}`/`SharedPtyTable` pattern now gives a concrete template for, not yet applied to
any of the three; not on the Xvfb/selkies boot path so lower urgency; (5) after (0)-(4),
`timerfd`/`signalfd` are the next-cheapest carriable fd kinds before attempting
`socket`/`unix-socket`/`epoll` (`pty` itself is no longer purely uncarriable — see the 36th pass:
a cross-process opener can now re-acquire a pty by id via `pts_open`'s shared fallback even though
the fd itself still isn't carried across `fork()`). (6) The general writable-layer-visibility gap for
LARGE/unbounded content (`/tmp/de.log`/`/tmp/de2.log`/`/tmp/xvfb.log`, `/tmp/wm1`/`/tmp/wm2` —
reconfirmed real, thirtieth pass: neither `[de]` nor `[de2]`-tagged output ever appeared in either
thirtieth-pass boot log despite `DE_FAILED` firing both times) remains open —
`SharedFilePublishTable`'s 256-byte cap must NOT be widened to try to cover it; a real fix needs its
own design (chunked publish, or a genuine shared-arena ring rather than a snapshot table).

## Container images and OCI loading

**`litebox_packager --oci-image <ref> --output <tar>`** pulls, whiteout-merges, rewrites every ELF and
produces a bootable flat tar in one command (x86-64/Apple Silicon hosts only) — supersedes the ad-hoc
OCI-pull Python scripts this project once hand-rolled, retired, do not recreate.

**Runtime in-memory loading** — `--oci-image <ref>` pulls, merges and rewrites every layer in memory; no
host directory is ever created for the rootfs (a real one hit three independent Windows-path bugs).
Rewritten layers are cached under `.litebox-cache/`, keyed so a rewriter change self-invalidates. Large
images (multi-GB, 100K+ entries) pack fine now; residual risk is host-memory contention, not a litebox
bug. `tar_ro.rs`'s multi-layer index is built ONCE at mount, not per read (was O(entries²), 17.3s →
0.35s fixed). Cache internals and the four fixed OOM bugs: archive.

A trampoline-extension failure used to poison a whole segment's syscalls, now fixed (archived).
Tags verified live, never from the name (archived): `linuxserver/webtop:alpine-mate` ships MATE not
XFCE; `alpine-xfce` doesn't exist; `debian-xfce`/`ubuntu-xfce` ship real XFCE.

**X server choice**: for the DRM/wgpu on-screen (`--gui`) path use `Xorg` with `modesetting` — litebox's
virtual DRM device is legacy-KMS + dumb-buffer + XRGB8888 only, no atomic modeset/GBM/EGL, so a GBM-first
compositor lands on its least-tested fallback, and `Xvfb` never touches DRM/KMS at all (zero page-flips,
indistinguishable from "never drew"). For browser/selkies, `Xvfb` IS correct and verified — its
`-shmem` framebuffer works now that SysV shared memory exists.

**Durable artifacts**: `C:\dev\litebox-webtop\webtop_seatd.tar` (stock MATE webtop); the
`.wfgy/xfce-build/` weston+XFCE tar is superseded by the stock-image path.

## A real desktop renders in a browser

**XFCE renders in a real host browser, and MATE too** — full pipeline (Xvfb, selkies/pixelflux
x264, MIT-SHM) inside litebox, reverse proxy host-side only. Working config: selkies
`--addr=0.0.0.0` port **8081**, dashboard over `--publish`, `/websockets` tunnelled to 8081.
Fourteen litebox defects got here, all landed (archive).

**A stock s6-overlay image boots with no flags/stubs**: `/init` runs 16 cross-process children
with zero uncarriable fds. **The black XFCE desktop was deterministic, now fixed**: the runtime
rewriter corrupted `libLLVM.so.19.1`'s `.dynsym`, so mesa `dlopen` failed forever. Rest settled
in the archive (PI futexes, labwc SIGABRT, gdk-pixbuf, network fixes).

**XFCE also renders on the THREAD-based fork path, gated by one flag** (`docker.io/linuxserver/
webtop:debian-xfce`, `.wfgy/webtop_stack.sh`). Without it, 3/3 boots die ~7s in — ADVISORY-001 §3N's
safe-linked-tcache write (the `fork_verify: stale CODE pointer` warning right before it is a red
herring; translation is correct). Fix: `--env GLIBC_TUNABLES=glibc.malloc.tcache_count=
0:glibc.malloc.mxfast=0` as an `--env` runner flag (a bare host-shell prefix does NOT reach the guest).
Glibc-only workaround, not a fix (PRD `glibc-tunables-workaround-pending-zero-fork`) for the
THREAD-based path specifically. **`LITEBOX_PROCESS_FORK=1` removes that whole crash class by
construction (no relocation, so no safe-linked-pointer mistranslation is possible) and, per the
35th pass above, no longer hits the old "Fork-after-Xorg" freeze either** — the AF_UNIX gap this
paragraph used to cite as the blocker was independently closed by the 13th/28th passes; the real
current blocker on EITHER fork path is `DE_FAILED` (see the 35th-pass pickup above), not AF_UNIX.
Selkies also needs `--clipboard-enabled=false` on the thread-based path (its clipboard monitor
re-triggers the same corruption every tick) — moot on the cross-process path for the same reason
`GLIBC_TUNABLES` is.

**Sixth/seventh pass (closed)** — writable-layer-adoption race fixed via the existing
atomic-rename primitive; live-verified 5/5 boots, zero recurrence. Detail: archive.

**Open here.** One client per selkies instance, no slot reclaim on reload. The AF_UNIX
connection-data-plane gap this paragraph used to describe is now DONE (thirteenth pass) and
crash-free through `DE_LAUNCHED` (twenty-eighth pass) — see the Cross-process fork section above
for the current sole blocker (`DISPLAY` loss across fork).

**The glibc/tcache crash class still sporadically hits selkies** on the THREAD-based fork path, a
SECOND corruption signature under heavy fork load (`double free or corruption (out)` SIGABRT).
Track B territory, not a tunable-coverage gap — do not re-attempt a `GLIBC_TUNABLES` fix without
evidence of a THIRD mechanism. Detail: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

### The ACK-stall-kill and port-8081 watchdog — both CLOSED (2026-09-16)

Real blocker was the guest-side patcher silently crashing on `shutil.copy2()`'s `copystat()` (no
`listxattr` shim) before ever patching `selkies.py`; fixed (`478e640`). Port-8081 double-bind fix
live-verified over 17 boot cycles + a 6000-connection stress test, zero recurrence. Detail: `docs/
AGENTS_ARCHIVE_2026-09-16.md`.

## Host-side crash machinery

A fatal host fault dumps before it dies, ungated (stack walk, `RECENT_FAULTS` ring, `RECOVERY_LOG`,
no env var needed); a real OS minidump comes only from the repeated-identical-fault circuit breaker
(`error_code` is synthesized, `is_in_guest` is tri-state — exact semantics: archive). An unexplained
`0xC0000005` may be a panic — the VEH handler enters only for the four codes it triages, registers
FIRST in the chain, sizes per-depth frames from disassembly not guesswork (`docs/veh-exception-
handler-design.md`). Cross-process sync on Windows is a hard platform constraint: every native
address/TID-based wait is process-local (`WaitOnAddress`, keyed events, `NtAlertThreadByThreadId`=
ACCESS_DENIED); only a shared kernel object crosses processes — `RawMutex` (below) is the one that
matters; `xproc_sync.rs`'s named-event primitive is live-verified but still unwired.

## Shared-memory foundations -- all DONE, live-verified 2026-09-16/17 (detail: those two archives)

`RawMutex`: no longer calls `WaitOnAddress`/`WakeByAddressSingle` (process-local per MSDN) -- manual
wait queue + one auto-reset kernel `Event` per OS thread, cross-process half real
(`DuplicateHandle`-based); gained `poisoned: AtomicBool` + owner-death recovery (eleventh pass,
above); `resolve_waiter_event`'s stale-pid panic fixed via a fixed-32-slot pointer-free
`WaiterQueue`. Shared kernel heap: a small **64 MiB, standalone, bounded**
`shared_kernel_arena_alloc` (`lib.rs`, NOT wired to `GlobalAlloc`) backs `SharedArc<T>`
(`value`+`strong: AtomicUsize`) for `LiteBoxX`/`GlobalState` placement; `SLAB_ALLOC` stays on the
private per-process path (a shared bump allocator exhausted an 8 GiB pool in 45-90 execs).
**Root cause of the whole `GlobalState`-sharing class, precisely characterized**: `SharedArc::new`
shares only `T`'s literal inline bytes -- any REGISTRY that was a `BTreeMap`/similar has its NODES
on the private per-process heap, meaningless to an attaching process (PRD:
`globalstate-nested-collections-not-actually-shared`). Of the original list (`unix_addr_table`,
`pty_registry`, `daemon_pty_masters`, `flock_registry`, `fifo_registry`, `sysv_shm`, `memfds`,
`shared_files`): `unix_addr_table`/`fifo_registry`/`memfds`/`shared_files` are now
per-process-shadowed on `GlobalStateHandle`, `sysv_shm` is a real shared-arena-native fixed array.
`pty_registry`/`daemon_pty_masters` are ALSO now per-process-shadowed (36th pass), with a genuine
cross-process companion restored separately via `syscalls::pty::SharedPtyTable` (a plain
`GlobalState` field, same pattern as `unix_addr_presence` below) rather than shadowed away, since
(unlike the other four) real cross-process pty visibility is actually needed. `flock_registry`
remains open (pickup list).
`SharedUnixAddrPresenceTable` (`syscalls/unix.rs`) established the reusable flat-table PATTERN
(`sysv_shm` and the AF_UNIX connection-DATA layer both reused it next): fixed-256-slot,
pure-atomic, lock-free `(kind, key bytes<=108, owner pid)` side-index. `Pipes.litebox`'s stale
pointer (fixed via `GlobalStateHandle::pipes()`) and `FutexManager`'s stack-allocated `LoanList`
sharing hang (fixed: each process gets its own fresh `FutexManager`) were two more instances of
the same raw-pointer-frozen-into-shared-bytes root pattern. `SafeZoneAllocator::alloc`'s spinlock
livelock (distinct mechanism, no dead-holder recovery unlike `RawMutex`) is still open.

## Closed — do not re-attempt without a genuinely new approach

VEH_FRAME_STRIDE canary guard, `dev_bench`/`litebox_runner_snp` build failures, CoW-mmap
performance, input-latency bugs, presenter-split duplicate-`SYN_REPORT`, the GUI-protocol
decision, five cheap-wins PRD rows, cross-process-fork stdio-handle bug (`spawn_suspended`'s
clobbered `STARTF_USESTDHANDLES`), presenter-process split (`docs/presenter-process-design.md`)
— all CLOSED, none open. Full detail: archive.

## Docs and tooling map

- **Archives** (newest first) — `_2026-09-18.md` (12th-34th passes, full trace/repro detail for
  everything AGENTS.md's own pass entries above summarize — shared AF_UNIX connection plane;
  DBUS_FAILED/DISPLAY-getenv()/AF_UNIX-errno all CLOSED; Xvfb's real SIGSEGV root-caused to a
  glibc memcpy reading an unmapped pointer, cdb refuted as a viable capture method, exact Xvfb
  call site still open (32nd); ldconfig static-PIE double-relocation SIGSEGV fixed (33rd);
  fork-eligibility mechanism confirmed by static reading, `LITEBOX_PROCESS_FORK=1` boot-wide flip
  confirmed still not a safe narrow win (34th)), `_2026-09-17.md` (shell-crash investigation, stdio-handle
  bug, 12 registry/pointer/lock fixes, writable-layer-race fix), `_2026-09-16.md` (popup-menu
  re-test, Track A audit, RawMutex/presenter), `_2026-09-15.md` (ACK-stall-kill), `_2026-09-10.md`
  (fork fd eligibility, OCI cache, s6-boot, browser config, crash-dump/VEH, CoW). Older:
  `_2026-09-03.md`, `_2026-09-05.md`.
- Fork: `docs/track-b-fork-fix-progress.md`, `advisor/ADVISORY-002-d-zero-fork.md`,
  `advisor/ADVISORY-001-fundamentals.md` (§3N tcache). `docs/veh-exception-handler-design.md` —
  read before touching VEH.
- Desktop logs: `docs/webtop-debian-selkies-2026-09-06.md`, `webtop-alpine-mate-2026-09-07.md`,
  `webtop-debian-xfce-2026-09-08.md`, `webtop-xfce-code-vs-data-2026-09-08.md`, `fork-fs-veh-2026-09-08.md`.
- Consult before deriving: `docs/premade-library-research.md`, `docs/drm-dumb-buffer-ioctl-reference.md`,
  `docs/diag-timeline-field-semantics.md` (before any `DIAG_TIMELINE` `comm`-field hypothesis).
- `docs/macos.md` — Apple Silicon guest-execution context switch is a stub, deferred. Designs NOT
  implemented: `docs/session-daemon-design.md`, `docs/fork-region-grouping-design.md`.
- `advisor/probes/` — diagnostics (`decode_frame.py`, `symbolize_litebox_crash.py`, `dup_probe.c`,
  `drm_flip_probe.c`, `clone_probe.c`, `socketpair_fork_probe.c`) plus `MEASUREMENT-PITFALLS.md`,
  `DISK-HYGIENE.md`.
- `.gm/memories/` — older per-topic notes, superseded by this file/archives.
