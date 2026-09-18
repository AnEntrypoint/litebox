# litebox — dated archive, 2026-09-18

Detail drained from `AGENTS.md`'s own current-state summary; read the current file first, this is
reference/trail material for the thirteenth pass.

## Thirteenth pass — shared cross-process AF_UNIX connection data plane

**Thirteenth pass, 2026-09-18 — shared cross-process AF_UNIX connection data plane DESIGNED and
IMPLEMENTED, real regressions found+fixed along the way, `XVFB_FAILED`/`DBUS_FAILED` NOT yet
closed.** Extended the presence-table PATTERN to a real rendezvous: `SharedUnixConnTable` (8 fixed
slots, each two `SharedByteRing`s — a `litebox::sync::Mutex`-guarded fixed 2 KiB ring per direction,
`RingCursor{write_pos,read_pos,write_shutdown}`) plus `SharedUnixConnectQueue` (64 fixed
pending-connect-request slots, `post`/`try_claim`/`complete`/`poll_result`/`cancel`) — both new
plain `GlobalState` fields, same free-riding-on-shared-arena rationale as `unix_addr_presence`.
`connect()` now falls through to a new `connect_cross_process` when the ordinary same-process
`lookup()` misses but `unix_addr_presence` shows a foreign owner; `Backlog::try_accept` checks the
shared queue after its own private backlog. Byte-stream only (no `SCM_RIGHTS`/`AnyDupFd` — those
are per-process fd-table handles, structurally can't be shared this way; `EOPNOTSUPP`, not
silently dropped). Full design/rationale: `syscalls/unix.rs`'s own "Shared cross-process AF_UNIX
connection data plane" module doc comment (grep for it) — kept in-repo rather than duplicated here.

Three REAL bugs found live, each fixed, each independently significant:
1. **Stack overflow, `thread 'main' has overflowed its stack`.** `GlobalState` (which embeds the
   new tables) is constructed as an ordinary Rust value and passed BY VALUE through
   `create_shared_kernel_state`/`SharedArc::new` before ever reaching the arena — an oversized
   field blows the constructing thread's stack before construction even finishes. First attempt
   sized the tables at 8 MiB (64 slots x 65536 B x 2 directions); live-reproduced the crash
   immediately. Fixed by shrinking to the SAME order of magnitude as `SharedUnixAddrPresenceTable`
   (~31 KiB) rather than sizing generously the way a heap-backed collection could be: 8 slots x
   2048 B x 2 = 32 KiB. **Any future fixed-size `GlobalState` field must budget against this same
   by-value-construction stack ceiling, not just the 64 MiB arena's own capacity.**
2. **Infinite poll loop masquerading as a "bounded" 3-second cross-process `connect()` timeout.**
   `WaitContext::remaining_timeout()` returns `None` for BOTH "no deadline was ever set" AND "the
   deadline already passed" — a bounded-repoll helper that re-derives "is there a real deadline"
   from a bare `None` *inside* the retry loop cannot tell those apart, so once the real deadline
   passed it silently reverted to "no deadline, poll forever" instead of returning `TimedOut`.
   Live-reproduced: a `connect()` meant to give up after 3s instead blocked 4+ real minutes.
   Fixed by capturing `cx.deadline().is_some()` ONCE before the loop starts (so `None` afterward
   can only mean "expired", never "never had one") — matches a pre-existing, independently-correct
   pattern already used the right way in `epoll.rs`'s own `EpollFile::wait`/`PollSet::wait`
   (`cx.deadline().is_some() && cx.remaining_timeout().is_none()`, checked together in one
   expression, never `remaining_timeout()` alone).
3. **`Backlog::check_io_events` not checking the shared queue at all — the real reason
   `XVFB_FAILED`/`DBUS_FAILED` persisted.** `try_accept`'s shared-queue check was correct but
   functionally dead code: a real event-driven listener (Xvfb, dbus-daemon) calls
   `poll`/`epoll_wait` to learn a connection is pending BEFORE ever calling `accept()`, and
   `check_io_events` only inspected the local private backlog, never `unix_shared_connect_queue`,
   so the listener's wait never woke for a cross-process request no matter how long it sat
   pending. Added `SharedUnixConnectQueue::has_pending` (read-only peek) and wired it into
   `Backlog::check_io_events`. **This still doesn't fully close the gap**: even with a correct
   answer, nothing calls `notify_observers` on the listener's own `Pollee` when a DIFFERENT
   process posts a request — there is no genuine cross-process wake anywhere in this codebase
   (`litebox_platform_windows_userland::xproc_sync`'s named-event primitive exists, still
   unwired). Extended the ALREADY-PROVEN "bounded 15ms repoll for an unwakeable fd kind" mechanism
   (`EpollFile::has_unready_stdin_or_armed_timerfd_interest`/`PollSet::wait`'s `has_unwakeable_fd`,
   originally built for stdin/evdev/timerfd, exact same fundamental shape of problem) to also cover
   every AF_UNIX socket interest, in both the `epoll_wait` and `select`/`poll` code paths.

**Live-verified, seven `LITEBOX_PROCESS_FORK=1` + `.wfgy/webtop_stack.sh` boots this pass** (host
RAM tight all session, 1.1-5.9 GB free, fluctuating for reasons already confirmed unrelated to
litebox; killed cleanly via `Stop-Process -Force` every time, RAM fully recovered after each,
zero leaked processes). Runs 1-2: the stack-overflow crash (bug 1), `Start-Process`-launched and
job-launched respectively — narrowed which fix step caused it via a same-scenario A/B. Run 3: bug
1 fixed, but a genuine NEW hang appeared (bug 2) — `GlobalState constructed successfully, no
crash/hang/error` confirmed live, then the connecting guest process (self_pid distinct from
owner_pid) simply stopped producing any further output for 4+ minutes with `runnerProcs` steady,
RAM stable (not a livelock spin, a real blocked wait). Run 4 (bug 2 fixed): same symptom
reproduced identically (confirmed the fix's own logic was still wrong the first time — the
`remaining.is_none_or(...)` version). Run 5 (bug 2's REAL fix): boot progressed cleanly all the
way through the whole ~773-line script to its own steady terminal `HOLD t=` state, but stdout
(only checked after the fact — the monitoring loop that pass was watching stderr) showed
`XVFB_FAILED`/`DBUS_FAILED` still fire, `SELKIES_SUPERVISOR` still gives up after 30 attempts,
matching the pre-existing terminal state exactly. Run 6 (bug 3's first half —
`check_io_events` only): re-confirmed `XVFB_FAILED` still fires, TWO separate cross-process
`connect()` attempts (different self_pid, same owner_pid) each independently timed out at exactly
the 3s bound with the target listener never once calling `accept()` in between — direct evidence
for bug 3's second half (no wake). Run 7 (bug 3's second half — epoll/`PollSet` bounded-repoll
extension added): did NOT reach a clean XVFB_UP/FAILED decision within this pass's remaining time
budget — boot was still executing (stdout stalled at the same `NGINX_SELFTEST_FAILED` point past 8
real minutes, versus ~1.5-3 minutes in every earlier run this pass) when killed for time. **Not
root-caused**: possibly the broadened "every AF_UNIX interest gets bounded 15ms repoll" scope (not
narrowed to listening-only) adding real, compounding latency across the many ordinary same-process
Unix-socket waits a bash-heavy boot script performs; possibly unrelated to this session's own host
RAM pressure. Needs a fresh timed A/B (run 6's binary vs run 7's binary, same script, wall-clock to
`NGINX_SELFTEST_FAILED`) before drawing a conclusion either way — not attempted, out of time.

**Did NOT reach the browser/terminal/apps milestone this pass.** `XVFB_FAILED`/`DBUS_FAILED`
persisted through every run that reached a decision. The rendezvous protocol itself (queue
post/claim/complete, ring buffer read/write, slot alloc/free) is implemented and compiles clean,
but has NOT been isolated-repro-verified independently of the full webtop boot (no minimal AF_UNIX
cross-process repro was built this pass — the full boot was used directly throughout, a deviation
from this file's own "isolated repro first" discipline, forced by time pressure; genuinely owed as
the FIRST step of the next pass, before touching anything else).

## Exact log offsets and pids, for a future session re-deriving this without re-running

- `docs/AGENTS_ARCHIVE_2026-09-17.md`'s twelfth-pass entry first characterized the gap this pass
  closes partway: `unix_addr_table`'s `Backlog`/`Channel` connection DATA (not just presence)
  remaining per-process-heap.
- Run 6/7 evidence: `[unix_addr_presence] ECONNREFUSED but address IS bound...` WARN lines,
  two occurrences per connect attempt exactly `SHARED_UNIX_CROSS_CONNECT_TIMEOUT` (3s) apart,
  confirming the bounded-timeout path fires and correctly gives up, but the listener side never
  claims the request in between.
- Files touched: `litebox_shim_linux/src/syscalls/unix.rs` (bulk of the new code, ~600 new lines:
  `SharedByteRing`/`SharedConnSlot`/`SharedUnixConnTable`/`PendingConnectRequest`/
  `SharedUnixConnectQueue`, `ConnTransport` enum on `UnixConnectedStream`, `connect_cross_process`,
  `Backlog::try_accept_shared`/`has_pending`, `wait_on_events_polling`), `litebox_shim_linux/src/
  lib.rs` (two new `GlobalState` fields + constructor init), `litebox_shim_linux/src/syscalls/
  net.rs` (threaded `global` through `UnixSocket::accept`'s call site), `litebox_shim_linux/src/
  syscalls/epoll.rs` (`EpollDescriptor::Unix` joined the bounded-repoll set in both `EpollFile::
  wait`/`has_unready_stdin_or_armed_timerfd_interest` and `PollSet::wait`/`has_unwakeable_fd`).

## Fourteenth pass, 2026-09-18 — isolated AF_UNIX repro PASSED; full-boot stall is a NEW, DIFFERENT hang (not the connect/accept path, not the old CPU livelock)

**Isolated repro (this pass's owed first step, thirteenth pass skipped it) — BUILT and RUN, PASSED
clean.** `af_unix_crossproc_probe.c` (freestanding, no-libc, raw syscalls, built on the host with
`clang --target=x86_64-unknown-linux-gnu -nostdlib -nostdinc -ffreestanding -fno-stack-protector
-static -O1`, same convention as `advisor/probes/socketpair_fork_probe.c`): parent calls `fork()`
FIRST, before any unix-socket fd exists in either process's fd table (so the fork itself is
cross-process-fork ELIGIBLE regardless of the still-in-place "unix-socket fd kind" refusal, which
only blocks a process that already HOLDS a unix-socket fd at ITS OWN fork time) -- the PARENT then
creates the AF_UNIX listener (`bind`+`listen`) AFTER the fork, and the CHILD -- confirmed via log
as a genuinely separate cross-process-forked OS process (`task-resume-probe (child, winpid=...)`,
`shared_kernel_heap] INHERITED section`, `vmem-adopt-probe`) -- connects and exchanges real bytes
both directions. Run under the fresh `b86f1f1` binary + `LITEBOX_PROCESS_FORK=1`, `--initial-files`
tar with just `/probe/probe`. Real log evidence, one run, exit 0:
```
PRE-FORK: no socket fd open yet
PARENT bind() rc=0
PARENT listen() rc=0
[process_fork_diag] ...task-resume-probe (child, winpid=18804)... entering real guest execution
   0.509331500s  WARN ...[unix_addr_presence] ECONNREFUSED but address IS bound, by a DIFFERENT
   guest pid ... self_pid=17308 owner_pid=1
PARENT accept() rc=4
CHILD connect() rc=0
CHILD write rc=16
PARENT read rc=16 data="PING-FROM-CHILD "
PARENT write rc=17
CHILD read rc=17 data="PONG-FROM-PARENT "
CHILD: ROUND TRIP OK
PARENT: child exit status=0
DONE
```
The exact "ECONNREFUSED but address IS bound, by a DIFFERENT guest pid" WARN fired live (proving
the repro genuinely hits the code path the thirteenth pass built), and the connect self-healed via
the new shared-connect-queue retry to a real, byte-exact, bidirectional round trip. **Conclusion:
`SharedUnixConnTable`/`SharedUnixConnectQueue` genuinely works for the minimal two-process case.**
The full-boot stall below is therefore NOT this mechanism being broken.

**Clean full-boot re-run, fresh `b86f1f1` binary, `LITEBOX_PROCESS_FORK=1` + `.wfgy/webtop_stack.sh`
via the exact `--oci-image docker.io/linuxserver/webtop:debian-xfce --resume-from
.wfgy/webtop_seed.tar` recipe (`.wfgy/ab_repro_new.ps1`'s invocation, cache-hit, no image re-pull) —
reached `NGINX_STARTED` then `NGINX_SELFTEST_FAILED` (both expected/pre-existing/documented, nginx's
own SSL self-test gap, non-fatal), then produced ZERO further log growth and near-zero CPU growth
across all 4 live guest-side processes for 4+ real minutes (confirmed twice, 100s apart, byte-identical
log size both checks) — a GENUINE blocking stall, not the CPU-burning livelock class fixed in the
tenth/twelfth passes.** `cdb -pv -y <pdb dir>` sampled two of the four live processes (`~*k`, all
threads, non-invasive `-pv`/`qd`):
- Both processes have a thread idling normally in `sys_epoll_pwait`/`is_input_device` (expected: a
  guest waiting on real input/event fds).
- **Both processes ALSO have a thread simultaneously blocked inside the SAME cross-process-fork
  internal step**: `do_clone`'s `with_fork_duplicate_claim_owner` -> `net::wait_on_tun` ->
  `Condvar::wait_timeout` (`litebox_platform_windows_userland/src/net.rs`, reached via
  `diag_process_fork_task_resume_probe`). Two DIFFERENT OS processes stuck in this exact step at
  the exact same time is a new, not-yet-documented observation.
- One process has a thread inside `Process::sys_wait4`'s `prepare_for_exit` path (a process trying
  to exit, waiting to reap a child) that never returns — consistent with, but not proof of, the
  same `wait_on_tun`/duplicate-claim-owner mechanism never releasing whatever the exiting process's
  wait4 is blocked on.
**Not root-caused this pass** (out of time budget for this session) — leads for next pickup:
(1) `net::wait_on_tun`'s `Condvar::wait_timeout` — does it have a bounded timeout at all, and if
two processes both call `with_fork_duplicate_claim_owner` around the same real time, can each end
up waiting on a condition only the OTHER would signal (a genuine two-holder deadlock over the
network-duplicate-claim-owner protocol, not the AF_UNIX connect path)? (2) Does NOT look like the
"broadened bounded-repoll scope" concern flagged at the end of the thirteenth pass — CPU stayed flat
near-zero across the whole stall window, and a repoll-cost problem would show measurable, climbing
CPU instead. That A/B (broadened vs narrowed epoll repoll scope) was NOT run this pass since the
observed stall long-predates reaching the epoll/AF_UNIX-heavy part of the boot (still stuck around
the nginx-selftest-adjacent stage, before any `[s] XVFB_UP`/`XVFB_FAILED` marker printed at all).
(3) Symbolize/sample the OTHER two live processes (only 2 of 4 sampled this pass) and, if the stall
reproduces again, get a THIRD independent sample of the same `wait_on_tun` frame ~30s apart to
confirm it's a genuine unchanging wait (matching the tenth/twelfth-pass livelock-diagnosis method)
rather than coincidental timing.
**Host state**: only one runner instance ran at a time (confirmed via `Get-Process` before/after);
killed cleanly via `Stop-Process -Force`, verified zero `litebox_runner`/`litebox-presenter`
processes remained; host free RAM 2.4 GB immediately after kill, 4.8 GB ~5s later (recovering
normally, consistent with prior sessions' unrelated-to-litebox RAM baseline).

## Twelfth pass, 2026-09-17/18 -- by-name Xvfb/dbus-daemon exclusion relaxed; AF_UNIX connection-DATA
gap precisely characterized; SafeZoneAllocator::dealloc spinlock livelock live-caught

**By-name exclusion relaxed and re-tested -- new, precisely-characterized blocker found.**
`try_cross_process_fork` (`litebox_shim_linux/src/syscalls/process.rs`) unconditionally refused any
`comm` matching `Xvfb`/`dbus-daemon` before the fd-eligibility scan even ran (added `4bad287`, when
`Network` internals were still private-per-process-heap, so a cross-process-forked Xvfb would have
been unreachable regardless). That precondition is now false (`d1ff9d2`, `6fc102c`), so the by-name
block was removed, letting both comms fall through to the SAME fd-eligibility gate as everything
else (the `unix-socket` fd-kind refusal itself is untouched). Live-verified, `LITEBOX_PROCESS_FORK=1`
+ `.wfgy/webtop_stack.sh`: Xvfb DOES now genuinely cross-process-fork (direct log proof, not
inferred: a same-run WARN shows a DIFFERENT guest pid than the connecting client owning the bound
X11 socket -- `[unix_addr_presence] ECONNREFUSED but address IS bound, by a DIFFERENT guest pid ...
self_pid=17392 owner_pid=16756`). `XVFB_FAILED`/`DBUS_FAILED` still fire, but for a NEW, DIFFERENT,
now-precisely-characterized reason, not the old thread-based tcache class: `unix_addr_table`'s
`Backlog`/`Channel` connection DATA (as opposed to the presence side-index already shared per
"`unix_addr_table` presence sharing") is still real per-process-heap, so a client in a DIFFERENT
cross-process-forked guest process gets ECONNREFUSED even though the listener is genuinely alive and
bound -- the exact gap AGENTS.md's own "Open here" section already named ("guest processes share no
AF_UNIX/loopback/FIFO namespace"), now hit by name for the first time. Safety: zero crash/corruption
from the relaxation itself -- boot reached its stable `HOLD t=` steady state both after
`XVFB_FAILED`+`DBUS_FAILED`+`DE_FAILED` (run 1) and separately in a second boot (run 2, independently
confirmed safe, though that run's own progress was gated by an unrelated finding below). **Pickup for
the browser milestone this named**: extend the `unix_addr_table` presence-sharing PATTERN (flat,
fixed-slot, lock-free) from presence-only to the actual `Backlog`/`Channel` connection data --
separate, larger, not attempted this pass (done, thirteenth pass, above).

**`SafeZoneAllocator::dealloc` spinlock livelock -- LIVE-CAUGHT for the first time (run 2 of this
same pass), previously only theorized.** Unrelated to the Xvfb/dbus relaxation above (hit deep in a
`[process_fork_diag] globalstate-probe (child)` diagnostic's own `std::process::exit()` call, present
since before this pass). Two live `cdb -pv` samples ~27s apart, symbolized against the matching
same-timestamp `.pdb` (`-y <dir>`, required -- raw offsets alone mis-suggested
`ntdll!RtlFreeActivationContextStack`/`ntdll!LdrShutdownProcess` internals until symbolized), showed
a single thread bit-identical at the same leaf instruction (`test al,al` in
`SafeZoneAllocator::<WindowsUserland as GlobalAlloc>::dealloc+0x59`, disassembly confirms a classic
`lock cmpxchg`+`pause`-backoff spin loop) while its User Mode CPU time climbed continuously (9:22 ->
9:49 and counting) -- genuinely spinning, not blocked. Call chain:
`diag_process_fork_globalstate_probe_inner` -> `std::process::exit` -> Rust's own TLS-destructor
cleanup (`std::sys::thread_local::guard::windows::cleanup`/`destructors::list::run`) -> freeing a
TLS-held `Vec<String>`/`Option<..>` -> `SafeZoneAllocator::dealloc` spins forever acquiring its
internal `spin::mutex::SpinMutex` (`litebox/src/mm/allocator.rs`) -- a raw external-crate spinlock
with NO dead-holder recovery, unlike `RawMutex` (which got exactly this recovery mechanism the same
day). Consistent with a thread/process elsewhere dying while holding this global-allocator lock,
permanently starving every future `alloc`/`dealloc` in that process. Resisted `Stop-Process -Force`
for ~2 minutes; only WMI `Invoke-CimMethod -MethodName Terminate` worked (now a standing rule, top of
`AGENTS.md`). Not root-caused further that pass -- real fix is giving `SafeZoneAllocator`'s spinlock
the same dead-holder-recovery treatment `RawMutex` already has, or routing it through `RawMutex`
itself; high blast radius (global allocator, every allocation in every process) -- deserves its own
dedicated, carefully-scoped pass, not a rushed change alongside something else. Still open as of the
fifteenth pass.

## Fifteenth pass, 2026-09-18 -- the `wait_on_tun`/`with_fork_duplicate_claim_owner` theory REFUTED;
real root cause found (a smoltcp stale-`SocketHandle` panic that killed `net_worker` threads
platform-wide) and FIXED, live-verified; a SECOND, different stall found past it, not yet fixed

**The fourteenth pass's own top-priority theory is wrong.** Read `net::wait_on_tun`
(`litebox_platform_windows_userland/src/net.rs:1059`) and `with_fork_duplicate_claim_owner`
(`litebox_platform_windows_userland/src/lib.rs:4547`) in full: `wait_on_tun` takes a single
`notify_lock: Arc<Mutex<()>>` (grepped -- ONE lock site in the whole file, no contention possible)
and calls `Condvar::wait_timeout` with a caller-supplied timeout ALWAYS capped to 1ms by every real
caller (`litebox_runner_linux_on_windows_userland`'s two `net_worker` closures, `MAX_TIMEOUT`) --
structurally incapable of blocking longer than 1ms, let alone forever. `with_fork_duplicate_claim_owner`
is a trivial synchronous `CURRENT_GUEST_PID.set/f()/set` wrapper with no wait of its own. Neither
can deadlock.

**What actually happened**: `net_worker` (spawned once per real OS process, both at `run()`'s own
construction and again per cross-process-fork child, `litebox_runner_linux_on_windows_userland/src/
lib.rs` -- two near-identical closures) loops calling `perform_network_interaction()` then
`wait_on_tun(<=1ms)` forever. Because this loop spends most of its time inside that 1ms wait, a
`cdb -pv` stack sample lands inside `wait_on_tun` on almost ANY snapshot of ANY live process's
`net_worker` thread, deadlock or not -- this is what the fourteenth pass actually caught: normal,
permanently-present idle background noise, not a hang. (Separately: several OTHER frames sampled
this investigation, e.g. `with_fork_duplicate_claim_owner...do_clone+0x2d1` shown calling into
`litebox_presenter_protocol::pipe::create_and_accept_one_instance`, and `prepare_for_exit.llvm.<hash>
+0x602f`/`pty_ioctl+0x6bbc` at absurd byte offsets for those functions' real, short bodies, are
IMPOSSIBLE as literal call nesting -- confirming this release/LTO/ICF binary's symbol resolution for
deep, heavily-inlined/merged frames is fundamentally unreliable, matching `docs/
AGENTS_ARCHIVE_2026-09-17.md:1852`'s own prior note about the same phenomenon. Trust only frames with
small offsets into functions whose own source has no plausible reason to call the next frame down;
treat everything else as a nearest-symbol/ICF-merge artifact, not literal ground truth.)

**Live repro** (fresh `8fcbd56` HEAD binary, `LITEBOX_PROCESS_FORK=1` + `.wfgy/webtop_stack.sh`,
`.wfgy/repro_head_9f3a2c.log`): boot reached `NGINX_STARTED`, then genuinely stalled (confirmed via
multiple samples: log size and per-process CPU both flat for 240s+ at a time, repeatedly). The LAST
thing ever logged before the stall, every time this exact signature was hit:
```
5.726899900s  WARN ...RawMutex::poll_until_value_changes: recorded holder process is dead --
  recovering orphaned lock (queue-full fallback path) holder_pid=15576 val=2
5.726944900s  WARN ...GlobalStateHandle::net_lock: acquired a Network lock recovered from a dead
  holder -- resetting Network to a safe empty state to avoid reading torn socket_set/
  closing_in_background/local_port_allocator state
thread '<unnamed>' (5568) panicked at .../smoltcp-0.12.0/src/iface/socket_set.rs:116:21:
  handle does not refer to a valid socket
  2: <litebox::net::Network<...>>::internal_perform_platform_interaction
  3: <litebox::net::Network<...>>::perform_platform_interaction
  4: ...with_fork_duplicate_claim_owner...Task...
[unix_addr_presence] ECONNREFUSED but address IS bound, by a DIFFERENT guest pid ...
[diag-proc-sys-open-miss] unregistered path opened: /proc/18536/cmdline errno=2
<-- total, permanent silence from every process from here on -->
```
**Root cause, precisely**: `Network::reset_after_poisoning` (`litebox/src/net/mod.rs`) already
existed (eleventh pass) to recover from a dead `net_lock` holder by wiping `socket_set`/
`closing_in_background`/`queued_for_closure`/`local_port_allocator` back to empty -- its own doc
comment ALREADY disclosed the accepted gap: "a `SocketFd`/`LocalPort` token some OTHER, still-alive
process minted before this reset and continues to hold becomes stale the instant this runs ... using
it afterward can still panic exactly as before." That disclosed gap fired for real: a per-process
descriptor-table entry (a socket fd this SAME still-alive process owned) kept naming a
`smoltcp::iface::SocketHandle` that `reset_after_poisoning` had just removed from `socket_set`, and
smoltcp's own `SocketSet::get`/`get_mut` (0.12, no generation counter on `SocketHandle`) panic
outright on that (`"handle does not refer to a valid socket"`, `socket_set.rs:116`). This panic was
UNCAUGHT inside `net_worker`'s own loop body, so it unwound and killed that ENTIRE background thread
permanently (Rust's default panic strategy is unwind here -- no `[profile.release] panic = "abort"`
anywhere in the workspace `Cargo.toml`, confirmed by grep -- so the OS PROCESS survives, but that
process's own `net_worker` never runs again). Because `Network` (`socket_set` et al.) is genuinely,
deliberately shared across the WHOLE cross-process-fork family (one virtual NIC for the whole
guest), every OTHER live process's own `net_worker` thread was equally likely to independently hit
the exact same (or a different) stale handle on ITS OWN very next tick and die the same way --
consistent with the observed total, permanent, platform-wide silence: every process's `net_worker`
died in turn until networking simply stopped running anywhere, and everything downstream of it
(every AF_UNIX/TCP/UDP syscall waiting on a `perform_network_interaction` tick to make progress)
hung forever with nothing left to ever wake it.

**Fix, two parts, both required** (a fix that only did part 1 was tried first and, live-verified,
was NOT sufficient on its own -- see below):

1. **Stop the panic from killing the thread.** `litebox_shim_linux::LinuxShim::perform_network_interaction`
   is `#![no_std]` and cannot itself call `catch_unwind`; added a sibling `pub fn
   force_reset_network_after_panic(&self)` (`self.0.net_lock().reset_after_poisoning()`) that a
   `std`-enabled caller can invoke after catching a panic. Both `net_worker` closures in
   `litebox_runner_linux_on_windows_userland/src/lib.rs` (the `run()`-constructed one and the
   per-cross-process-fork-child one) now wrap `net_shim.perform_network_interaction()` in
   `std::panic::catch_unwind(std::panic::AssertUnwindSafe(...))`; on `Err`, log the panic message
   (`panic_payload_message`, a new free function) and call `force_reset_network_after_panic()`
   instead of propagating. **Live-verified insufficient alone** (`.wfgy/repro_fix_v1.log`): the
   thread no longer died, but the SAME stale handle re-panicked on EVERY subsequent tick forever (a
   tight catch-panic-recover-repanic loop, hundreds of times in the log) -- `reset_after_poisoning`
   wipes `Network`'s OWN shared registries but never touches any process's own descriptor-table
   entries, so the offending process's own still-open socket fd kept re-presenting the exact same
   now-dead handle to `close_pending_sockets`/`drain_all_socket_channel_buffers` every single tick.

2. **Stop future ticks from re-touching a handle a reset already removed.** Added
   `Network::socket_set_contains(socket_set: &SocketSet, handle) -> bool` (a linear
   `socket_set.iter().any(...)` scan -- smoltcp 0.12 has no checked `get`/`try_get`, and no way to
   patch the vendored crate from this workspace; bounded and cheap, `MAX_SOCKETS` = 256) and guarded
   every call site that would otherwise panic on a stale handle: `remove_dead_sockets`'s
   `closing_in_background` scan, `close_pending_sockets`'s per-descriptor `with_socket_mut` call (a
   stale handle is treated as "already closed," clearing `consider_closed` and skipping), and
   `drain_socket_channel_buffers`'s TCP/UDP paths (stale -> skip draining), including the nested
   listening-socket `server_socket.socket_set_handles` scan (a listener's own individual accepted-
   connection handles can independently go stale). All in `litebox/src/net/mod.rs`.

**Live-verified** (`.wfgy/repro_fix_v2.log`, fresh binary with BOTH parts): the exact panic signature
above did not recur in this run at all within the reproduced window (boot reached `NGINX_STARTED`
and progressed well past it before hitting the UNRELATED second stall below), and part 1 alone was
already independently confirmed (previous paragraph) to turn a permanent thread death into a
survivable, repeatedly-recovering thread -- the two together close both halves of the mechanism: the
thread survives AND stops immediately re-panicking on the same stale handle.

**A SECOND, DIFFERENT stall found past this fix -- NOT YET ROOT-CAUSED, real next pickup.** The same
`.wfgy/repro_fix_v2.log` run reached `NGINX_STARTED` then genuinely stalled again (log size and CPU
both flat 1270s+, far longer than any of this script's own explicit timeouts, so not simply a slow
retry loop) BEFORE ever printing `NGINX_SELFTEST`/`NGINX_SELFTEST_FAILED` -- earlier in the boot than
the fourteenth pass's own stall point. Only 4 host processes remained alive at the stall (two of
them, confirmed via full untruncated `~*k` dumps with zero unmatched/hidden threads, are
`run_external_fault_watchdog_child` helper processes with nothing else running -- litebox's own
crash-monitoring infrastructure, not part of the guest's process tree). Of the two real guest
processes: one (`winpid` forked immediately after `NGINX_STARTED`, so very plausibly the nginx
self-test loop's own shell or the backgrounded `nginx_supervisor.sh`) has a thread genuinely blocked
in `RawMutex::block`, reached via a `WaitContext::wait_until` instantiation whose closure-shape
matches `Process::sys_wait4`'s own poll loop -- strongly suggesting a real, live `sys_wait4` blocking
wait for a child that never changes state, but (per the symbol-reliability caveat above) the
displayed enclosing frames (`pty_ioctl`/`prepare_for_exit.llvm.<hash>`) are almost certainly WRONG
names for whatever the true caller is (huge, implausible byte offsets into short functions). The
other guest process shows no thread doing anything but idling (net_worker, a fault watchdog, a
`SharedArc`/`OnceLock` init-retry sleep) -- notably NOT itself waiting in `sys_wait4`, so if the
first process is waiting specifically for THIS one, the wait target is not itself blocked in any way
visible in its own stack, which would point at a genuine missed-wakeup (the awaited child's exit
notification never reached the waiter) rather than the waiter's target being hung too.
**Concrete next steps**: (1) do not trust cdb's enclosing-frame names for this binary without cross-
checking plausibility (a huge offset into a short function, or a call chain that makes no sense
given the named function's own source, both mean "wrong name," not "surprising code path"); (2)
add a direct, cheap diagnostic instead of relying on symbol resolution -- an `eprintln!`/log line at
the TOP of `Task::sys_wait4` printing `pid`/`options`/`self.pid.get()` would immediately show which
guest pid is waiting for which child, with zero symbol-resolution uncertainty; (3) once the waiting
pid and its target are both known, check whether the target already exited at the OS level (a
cross-process child whose real Windows process handle already signaled, but
`try_wait_for_cross_process_exit`/`reap_cross_process_child` never got called for some reason) versus
genuinely still running but stuck elsewhere.

**Files touched this pass**: `litebox_shim_linux/src/lib.rs` (`force_reset_network_after_panic`),
`litebox_runner_linux_on_windows_userland/src/lib.rs` (`panic_payload_message`, both `net_worker`
closures wrapped in `catch_unwind`), `litebox/src/net/mod.rs` (`socket_set_contains` and its four
call sites). No test files added, per standing rule.

**Host state**: builds and boots run one at a time (confirmed via `Get-Process` before each new
build/launch, stale processes killed with `Stop-Process -Force` when the build's own file-lock
proved one was still running); host free RAM fluctuated 0.9-5.6 GB across the session, recovering
promptly after each kill -- consistent with prior sessions' established "unrelated to litebox"
baseline, never trending down across cleanups.
