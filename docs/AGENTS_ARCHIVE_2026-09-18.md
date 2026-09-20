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

**Third run this pass, with `litebox_shim_linux::syscalls::process=debug` -- the second stall did
NOT reproduce; confirms the fifteenth-pass fix, and separately reconfirms the pre-existing,
already-tracked AF_UNIX connection-DATA gap is what still blocks the browser milestone, not a new
regression.** Fresh binary (both fix commits included), same repro. This run's own early section hit
a SIMILAR-looking ~120s pause right after `NGINX_STARTED` (matching the second stall's location) but
self-resolved on its own without intervention -- consistent with this whole area being genuinely
probabilistic, as repeatedly established all session, rather than the second stall being deterministic.
Progressed FAR further than any other run this pass: `NGINX_STARTED` -> `XVFB_FAILED` ->
`DBUS_FAILED` -> selkies launched and retried 30x -> `DE_LAUNCHED (image startwm.sh)` ->
`DE_FALLBACK_LAUNCHED` -> settled into the script's own steady-state `[s] HOLD t=<n>s` loop (reached
`t=480s` before this pass ended it), 559 total cross-process forks, 30+ MB of debug log, zero
uncaught panics, zero permanent stall. **`XVFB_FAILED`/`DBUS_FAILED` root cause, directly confirmed
in context**: the exact `[unix_addr_presence] ECONNREFUSED but address IS bound, by a DIFFERENT
guest pid` WARN fired for `xset`'s own connect to Xvfb's X11 socket (`self_pid=12892
owner_pid=2380`), then `webtop_stack.sh`'s own 60s `XSOCK` poll loop gave up and killed both `Xvfb`
and the pending `xset q` -- i.e. the thirteenth-pass `SharedUnixConnTable`/`SharedUnixConnectQueue`
mechanism (proven sound in the fourteenth pass's minimal isolated repro) is NOT actually resolving
this connect for Xvfb's real socket in the full boot, for a reason the isolated repro's success does
not explain -- matching this file's own thirteenth-pass entry ("did NOT reach the browser/
terminal/apps milestone... `XVFB_FAILED`/`DBUS_FAILED` NOT yet closed") and AGENTS.md's own current
"Open here" section precisely. This is NOT a new regression from this pass's fix and NOT the second
stall from earlier in this same pass -- a separate, pre-existing, already-tracked gap. Tried
`Invoke-WebRequest http://127.0.0.1:8080/` against the published nginx port while the run sat in its
`HOLD` state: timed out (nothing real being served, consistent with a genuinely non-functional
X/selkies backend) -- did not attempt a browser/CDP connection given this direct evidence the
backend has nothing to show. **Concrete next step for the browser milestone**: instrument
`connect_cross_process`/`SharedUnixConnectQueue::post`/`try_claim` (`litebox_shim_linux/src/
syscalls/unix.rs`) with the same kind of direct pid/path logging used for the fourteenth-pass probe,
run this EXACT full-boot repro (not another isolated probe -- the isolated repro already passed and
does not reproduce this), and find where Xvfb's specific listener socket path diverges from the
probe's own connect/accept sequence.

## Sixteenth pass, 2026-09-18 -- the "60s XSOCK timeout killed the shared-queue mechanism" theory
REFUTED; real bug was the boot script's own new readiness check; FIXED and live-verified; a SECOND,
different, not-yet-root-caused `xset` kill found immediately past it

**Followed the fifteenth pass's own concrete next step, and found something upstream of it
instead.** Before instrumenting `connect_cross_process` itself, re-read `.wfgy/webtop_stack.sh`'s
own wait loop (lines 274-280 as of the fifteenth pass) that gates the `xset q` liveness probe:

```
XSOCK="/tmp/.X11-unix/X${DISPLAY#:}"
i=0
while [ $i -lt 60 ]; do
  [ -S "$XSOCK" ] && break
  i=$((i+1)); sleep 1
done
xset q > /dev/null 2>&1 && echo "[s] XVFB_UP" || echo "[s] XVFB_FAILED"
```

`[ -S ... ]` is a POSIX socket-file-TYPE test. Checked the filesystem layer with no hypothesis
involved: `litebox::fs::FileType` (`litebox/src/fs/mod.rs:271-278`) enumerates exactly
`RegularFile`/`Directory`/`CharacterDevice`/`Symlink`/`Fifo` -- there is no `Socket` variant at
all. `UnixSocketAddr::bind`'s server-side path-creation branch (`litebox_shim_linux/src/
syscalls/unix.rs:107-140`) calls `fs.open(path, OFlags::CREAT|EXCL|RDWR, mode)` -- an ORDINARY
regular-file create, with its own `// TODO: extend fs to support creating sock file (i.e., with
type InodeType::Socket)` comment sitting right there disclosing the gap. `sys_mknodat`
(`litebox_shim_linux/src/syscalls/file.rs:1181-1185`) confirms independently: `InodeType::Socket
| InodeType::BlockDevice | InodeType::CharDevice | InodeType::Dir => return Err(Errno::EPERM)`,
with its own `// TODO: socket, block and char files are not supported` comment. So `lstat()` on
ANY litebox-bound AF_UNIX path reports `S_IFREG`, never `S_IFSOCK` -- structurally, unconditionally,
regardless of whether Xvfb is genuinely listening. `-S "$XSOCK"` can never be true.

**This readiness check is brand new, added THIS SAME DAY** (its own comment block, lines 257-273,
says so explicitly): it replaced a `while ...; do xset q ...; done` retry loop specifically to stop
re-exec'ing `xset` up to 60 times per boot (a thread-based-fork tcache-corruption concern from
before Xvfb/dbus-daemon's by-name cross-process-fork exclusion was relaxed, twelfth pass). So the
fourteenth/fifteenth passes' own "60s XSOCK poll loop gave up and killed both Xvfb and the pending
`xset q`" read was real (that IS what the log showed) but mis-attributed the STALL to the shared-
queue rendezvous mechanism (`SharedUnixConnTable`/`SharedUnixConnectQueue`) never resolving in time
-- the mechanism never even got a fair chance to run before this fix, because the loop unconditionally
burned its whole 60s on every single boot first, every time, regardless of Xvfb's real state.

**Fix**: changed `[ -S "$XSOCK" ]` to `[ -e "$XSOCK" ]` in `.wfgy/webtop_stack.sh` -- mere path
existence is the one thing `bind()`'s `CREAT|EXCL` actually guarantees once Xvfb has bound the
address, and `-e` is still a bash builtin (zero forks while waiting, preserving the exact property
the `-S` optimization was going for). Safe to drop the exec-avoidance concern that motivated `-S`
in the first place: this session already runs under `LITEBOX_PROCESS_FORK=1` (real cross-process
fork, not the thread-based/relocating path the corruption class needs) with the global
`GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0` export already covering every
future exec'd process regardless.

**Live-verified**, fresh HEAD binary (`30e0022`) + `LITEBOX_PROCESS_FORK=1` +
`.wfgy/webtop_stack.sh` with the fix, `--oci-image docker.io/linuxserver/webtop:debian-xfce
--resume-from .wfgy/webtop_seed.tar` (seed tar regenerated from the fixed script; the old one is
kept as `.wfgy/webtop_seed_stale.tar`): the Xvfb stage's `xset` connect attempt happened well inside
the first real second (`0.666113000s WARN ... self_pid=19484 owner_pid=8040` -- a genuinely
DIFFERENT pid, confirming real cross-process fork, not the old thread-based path), never burning
the dead 60s wait at all. The boot then progressed through the SAME known-good trajectory as the
fifteenth pass's best run: `SELKIES_SUPERVISOR` respawned and gave up after 30 attempts (root cause
directly visible in-line: `/tmp/selkies_supervisor.sh: 19: cannot open /tmp/empty: No such file` --
the ALREADY-TRACKED `/tmp/empty` writable-layer cross-child-visibility gap, Track B pickup (3), not
a new regression) -> `SELKIES_PORT_SELFTEST_FAILED after 170s` -> `DE_LAUNCHED (image startwm.sh)`
-> `DE_VIA_STARTWM=no` -> `DE_FALLBACK_LAUNCHED` -> `DE_FAILED` -> stable `[s] HOLD t=20s`+ with zero
panics and zero permanent stall. `Invoke-WebRequest http://127.0.0.1:8080/` still timed out (nginx
has nothing real to proxy while selkies never successfully starts) -- browser milestone NOT reached
this pass.

**A SECOND, different, NOT-YET-ROOT-CAUSED blocker sits immediately past this fix.** The exact same
`/webtop_stack.sh: line 290:   149 Killed                  xset q > /dev/null 2>&1` signature the
fourteenth/fifteenth passes already saw (and `docs/AGENTS_ARCHIVE_2026-09-17.md:2229` saw even
earlier, when Xvfb/dbus/xset were still thread-based-fork-only) still fires -- but now, immediately
after a WARN that proves `xset` genuinely cross-process-forked into a different OS process
(`self_pid=19484`), not the thread-based path the 09-17 archive blamed. Unlike every OTHER forked
child visible in the same log (which each show `run_thread returned (guest thread terminated)` ->
`exported writable layer to ...` -> `exiting with encoded status 0xc0deNNNN`), pid 19484's own
sequence stops dead right after the WARN and the `[diag-proc-sys-open-miss]` line -- no exit
diagnostic at all. litebox's own ungated fault machinery (`RECENT_FAULTS`/`RECOVERY_LOG`/minidump,
ordinarily unconditional per this file's "Host-side crash machinery" section) logged NOTHING for
this event either, which argues against (but does not fully rule out) a host `0xC0000005`-class VEH
-caught fault of the kind that machinery already catches. Grepped the full log for `panic`/
`0xC0000005`/`WER`/`abort` around this exact point: nothing. **Not root-caused this pass** -- a
one-shot, ~1-second-lived forked child is hard to catch with a debugger after the fact; the
concrete next step is either (a) a deliberate short `sleep` shim placed in front of the real `xset`
binary (e.g. a wrapper script substituted via `PATH`, mirroring the existing `dbus-launch` shim
technique already used elsewhere in this same script) so `cdb -pv` can attach BEFORE the kill, or
(b) direct temporary logging inside `connect_cross_process`/`wait_on_events_polling`
(`litebox_shim_linux/src/syscalls/unix.rs`) bracketing every real syscall it performs, since the
WARN's own `self_pid` matches the killed pid exactly, meaning the fatal event happens somewhere
inside or immediately after that exact function on this exact process.

**Files touched this pass**: `.wfgy/webtop_stack.sh` only (`-S` -> `-e`; gitignored, not committed
to git as tracked source -- consistent with every other repro artifact in this directory). No Rust
source changed. `.wfgy/webtop_seed.tar` regenerated from the fixed script (old copy preserved as
`.wfgy/webtop_seed_stale.tar`). No test files added, per standing rule.

**Host state**: one runner instance at a time throughout (confirmed via `Get-Process` before/after
every launch); killed cleanly via `Stop-Process -Force`, verified zero `litebox_runner`/
`litebox-presenter` processes remained; host free RAM 5.09GB before the run, 2.86GB immediately
after a forced kill of an 8-process fork family, recovering to 5.19GB within 5 seconds -- consistent
with every prior session's own "RAM pressure is transient and unrelated to litebox" baseline.

## Seventeenth pass, 2026-09-18 -- `xset q`'s silent kill CAUGHT LIVE, root-caused, FIXED; a new, deeper stall found immediately past it

**Method**: launched the runner with `cdb -o -g -G -cf <script>` (Debugging Tools for Windows,
`x64\cdb.exe`) so the whole cross-process-fork child tree is auto-attached from process start
(`-o` = debug child processes too; `-g -G` = skip the initial/final breakpoints). The script
silences routine noise so only genuine faults break in: `sxn sse` (litebox's own `fork_verify`
single-step healing traps constantly and is expected), `sxn ld`/`sxn ud` (module load spam),
`sxn ct`/`sxn et` (thread create/exit spam), `sxn eh` (Rust's own MSVC-target unwind machinery
raises a first-chance `e06d7363` C++ EH exception on every panic-unwind -- expected, not a crash),
`sxn c0000008` (STATUS_INVALID_HANDLE -- Windows raises this as a first-chance exception on
`CloseHandle` of an already-closed handle ONLY when a debugger is attached; harmless double-close
noise the codebase doesn't see without `cdb` attached at all -- note the cdb mnemonic for this is
NOT `ii`, use the raw hex code). `av`/`gp`/`asrt`/`bpe` are left as `sxe` with a short auto-
continuing diagnostic (`.exr -1; r; g` -- `.ecxr`+`kv`+disassembly were tried first and dropped:
`.ecxr` reliably failed with "Unable to get exception context, HRESULT 0x8000FFFF" on these
single-step-adjacent traps, and the extra output roughly quadrupled log volume for no additional
signal). `RUST_BACKTRACE=1` was set on the runner's own environment (not just `--env` into the
guest) specifically so a HOST-side Rust panic prints its full stack trace into the same combined
log cdb writes to.

**First finding (initially mis-read as `xset`-specific, then generalized): a benign, already-
working AV-based healing loop.** The first AV caught this way was NOT a crash: a cross-process-
fork child hit a `fs:[0x28]`-relative (stack-canary-check-shaped) access violation repeatedly
(12-14 times, same thread, same RIP, single-step then AV each time) on an ordinary fork child
(script byte offset 4879, nowhere near `xset`), then resolved cleanly -- `run_thread returned
(guest thread terminated)` -> `exported writable layer` -> `exiting with encoded status
0xc0de0000`, the same clean-exit pattern every successful fork child shows. This is
`vectored_exception_handler`'s own documented AV-path stale-pointer healing
(`litebox_platform_windows_userland/src/fork_verify.rs`) working as designed, caught live for the
first time simply because nothing had instrumented a child this deeply before. A separate, also-
benign AV recurred ~14 times across the whole boot in the TOP-LEVEL PARENT at one fixed address,
once per completed fork child -- also never fatal. Neither is the mechanism this pass hunted;
noted so a future pass doesn't re-investigate them as new leads.

**Second finding, the real one: `sed -i` (script lines 90-94, `webtop_stack.sh`'s own nginx-config
`sed` calls, much earlier than `xset`) dies with the exact same `bash: ... Killed` signature the
whole investigation had been chasing for `xset` specifically.** Caught the moment it happened:
immediately preceding the `Killed` line, cdb's own `RUST_BACKTRACE=1`-driven panic print showed

```
thread '<unnamed>' (21496) panicked at
/rustc/48a229ceaefd4985c50990b14116b6d856af0985/library\alloc\src\collections\btree\node.rs:1232:35:
range end index 25710 out of range for slice of length 11
```

with a full backtrace through `alloc::collections::btree::map::BTreeMap<..., MemfdEntry, ...>::
insert`, `litebox_shim_linux::syscalls::mm`, `litebox_shim_linux::syscalls::file`,
`LinuxShimEntrypoints`, `litebox_platform_windows_userland::diag_mm_enabled`, `syscall_callback`,
`run_thread_inner`, `run_thread_with_fork_verification`,
`litebox_runner_linux_on_windows_userland::diag_process_fork_globalstate_probe_inner`. The exact
same panic message, byte-identical ("25710... length 11" every time, never a different number --
a frozen stale value being read back, not random heap garbage), recurred on three more independent
forked children across two separate `cdb`-instrumented boots, always inside
`syscalls::mm::try_memfd_mmap`/`try_shared_file_mmap` (confirmed by reading those two functions:
both do `self.global.memfds.lock().get_mut(&key)` / `self.global.shared_files.lock()...
insert(...)`, on literally EVERY `mmap()` of any file-backed fd -- i.e. every exec'd guest binary's
own dynamic linker mapping its shared libraries at startup hits one of these two call sites,
unconditionally, regardless of whether the fd is actually a memfd). This unwinds (via Rust's
MSVC-target SEH-based panic-unwind, the `e06d7363` C++ EH exception silenced above) to
`diag_process_fork_globalstate_probe`'s `.spawn(diag_process_fork_globalstate_probe_inner)
.expect(...).join().expect(...)` (`litebox_runner_linux_on_windows_userland/src/lib.rs:1253-1258`)
-- the `.join()` returns `Err`, and `.expect("cross-process fork child's guest-execution thread
panicked")` panics a SECOND time, this time on that process's own `main` thread, with nothing
further up the call stack to catch it (confirmed: the second panic's own backtrace shows
`std::rt::lang_start_internal`'s `catch_unwind` -- Rust's own runtime entrypoint wrapper around
`main`, not an application-level catch -- immediately below `main`). Rust's default behavior when
`main` itself panics uncaught is to print the message (already captured above) and call
`std::process::exit(101)`: a clean, fully controlled process exit, NOT a hardware fault of any
kind. This is the concrete, live-confirmed answer to why litebox's own VEH-based
`RECENT_FAULTS`/`RECOVERY_LOG` crash machinery (`litebox_platform_windows_userland`, "Host-side
crash machinery" section) showed nothing for this class of death every previous pass: there is no
exception for it to intercept. The visible-to-the-guest symptom (`bash: ... NN Killed ...`, zero
further diagnostic) comes from a separate, still-open gap one layer further out: the PARENT's
`wait4()`-emulation path apparently maps any unrecognized host child exit code (101 here, vs. the
`0xc0deNNNN` sentinel family a clean guest-thread exit uses) to a synthetic "killed by signal"
status for the guest rather than surfacing the real exit code -- worth a dedicated future pass in
its own right, independent of the panic itself being fixed (below), since ANY future uncaught
host-side panic in ANY guest-reachable path will still show up this same opaque way.

**Root cause, precisely**: `GlobalState::memfds`/`GlobalState::shared_files` (both
`litebox::sync::Mutex<Platform, syscalls::mm::MemfdRegistry<Platform>>`, i.e. plain
`BTreeMap<(usize, usize), MemfdEntry<Platform>>` under a lock) were still raw fields of the shared
`GlobalState` struct placed byte-for-byte in the cross-process shared kernel arena. This is the
identical defect class `GlobalStateHandle`'s own doc comment already documents six separate times
(`litebox`, `proc_self_info`/`pts_registry`, `elf_patch_cache`, `exec_ranges_cache`,
`segment_scan_cache`, `futex_manager`): `SharedArc::new`/`create_shared_kernel_state` shares only a
value's literal inline bytes, and a `BTreeMap`'s inline bytes are just a root pointer + length --
meaningless (in `elf_patch_cache`'s case, this exact `btree::node.rs:1232` panic, previously
diagnosed 2026-09-17) in an ATTACHING cross-process-fork child's own address space, which never had
those specific heap pages mapped at all. `memfds`/`shared_files` simply hadn't been hit by this
yet, because nothing before this pass had a cross-process-fork child perform a file-backed `mmap()`
early enough in its life to reach `try_memfd_mmap`/`try_shared_file_mmap` while instrumented
closely enough to notice -- `elf_patch_cache` et al. are touched by `execve` itself (universal,
found immediately, 2026-09-17), while these two are touched only by `mmap()` (only found now,
because `sed`'s own dynamic linker's library-mapping `mmap()` calls happened to be the first
sufficiently-early, sufficiently-common trigger this investigation's tooling caught in the act).

**Fix, identical shape to the six prior instances**: removed `memfds`/`shared_files` from
`GlobalState` (replaced with a `NOTE` comment matching the `litebox`/`futex_manager` ones, pointing
at this section). Added `memfds: Arc<litebox::sync::Mutex<Platform, syscalls::mm::MemfdRegistry
<Platform>>>` and `shared_files: Arc<litebox::sync::Mutex<Platform, syscalls::mm::MemfdRegistry
<Platform>>>` to `GlobalStateHandle` itself, constructed fresh (`my_memfds`/`my_shared_files`,
empty `BTreeMap::new()` wrapped in a fresh `Mutex` wrapped in a fresh `Arc`) in
`LinuxShimBuilder::build()` before the create-vs-attach branch, alongside `my_elf_patch_cache` et
al. -- so every process, create or attach alike, gets its own private, always-valid, always-
correctly-constructed instance. Every existing `self.global.memfds`/`self.global.shared_files` call
site (`litebox_shim_linux/src/syscalls/mm.rs:839,1031`, `syscalls/file.rs:1053,1118`) needed no
changes at all: Rust's field-resolution rules try the receiver's own concrete type
(`GlobalStateHandle`) before auto-`Deref`ing to `GlobalState`, so the new field transparently
shadows the removed one, exactly as documented for all six prior instances. `Clone` for
`GlobalStateHandle` updated to clone the two new `Arc`s. Build: `cargo build --release -p
litebox_runner_linux_on_windows_userland`, clean, zero errors, one pre-existing unrelated warning
(`live_pty_ids` dead-code, not touched by this change), 42.63s.

**Accepted, explicit tradeoff** (identical in kind to `futex_manager`'s own documented one): a
memfd created by one process in a cross-process-fork family, or a `MAP_SHARED` mapping of an
ordinary file, is no longer visible to another member of that same family via this specific
mechanism. This was never actually working before this fix (it panicked the reader instead of
sharing anything), so this is a strict improvement, not a regression -- the WITHIN-one-process
`dup()`/thread-fork sharing rationale `memfds`'s own doc comment describes is completely unaffected
(it never crossed a `GlobalStateHandle` instance to begin with).

**Live-verified, twice, clean (no `cdb`, no debug overhead)**: fresh binary,
`LITEBOX_PROCESS_FORK=1` + `LITEBOX_LOG=warn,litebox_platform_windows_userland::fork_verify=error`
+ `RUST_BACKTRACE=1`, `.wfgy/webtop_stack.sh` (`-e`-fixed, sixteenth pass) unchanged,
`--oci-image docker.io/linuxserver/webtop:debian-xfce --resume-from .wfgy/webtop_seed.tar`. Both
runs: the fork resuming at script byte offset 19217 (`xset`'s own line) shows the same clean
`task-resume-probe (child): run_thread returned (guest thread terminated)` -> `exported writable
layer` -> `exiting with encoded status 0xc0de0000` sequence every other successful fork child in
the log shows -- no panic, no `Killed`, no exception of any kind at that point in either run. Zero
`sed`/`xset`-class panics anywhere in either full boot log up to the point each run reached (see
below). This closes the entire investigation this multi-day session's "xset q silent kill" question
was about: real mechanism identified with direct live evidence (a genuine Rust panic->clean-exit,
not a hardware fault, not a gap in the VEH machinery itself), fixed at its actual root cause (not
worked around), and the fix live-verified to actually stop it from recurring.

**A different, deeper, NOT-YET-ROOT-CAUSED stall found immediately past this fix, deterministic
2/2.** Both live-verification boots above progress cleanly past `xset` (offset 19217) into the very
next fork child (offset ~19289 -- `xrdb "$HOME/.Xresources"` per the script's own next line, though
not yet confirmed by direct comm-name evidence) and then stop making any further forward progress
at all: no more log lines of any kind, and the specific winpid's own CPU time barely moves across a
5-second `Get-Process` sample (9.703125s -> 9.765625s -- ~1.25% of one core, i.e. blocked, not
spinning). A third run, this time with `LITEBOX_LOG=...,litebox_shim_linux::syscalls::unix=debug`
added specifically to get this module's `TRACE unix_connect: entry`/`: result` lines, reproduced
the identical stall at the identical point and revealed the connect target is an ABSTRACT-namespace
address (`Abstract([47, 116, 109, 112, ...])`, i.e. bytes starting `/tmp` -- not the X11 socket,
which is path-based, not abstract; likely a D-Bus-family or compositor IPC socket), and that
`connect()`'s cross-process branch (`connect_cross_process`, `litebox_shim_linux/src/
syscalls/unix.rs:1354`) is reached, immediately logs the already-known `log_cross_process_
presence_miss` WARN (`[unix_addr_presence] ECONNREFUSED but address IS bound, by a DIFFERENT guest
pid`), and then nothing else is ever logged for that process again. Two candidate mechanisms were
read (both look correct on inspection, neither confirmed live yet): (a) if the `unix_addr_presence`
re-check inside `log_cross_process_presence_miss` finds a different result than
`connect_cross_process`'s own immediately-preceding check (a TOCTOU race between the two separate
atomic lookups), the control flow this pass read statically may not match what actually executes;
(b) `SHARED_UNIX_CROSS_CONNECT_TIMEOUT` (3 seconds, `unix.rs:2689`) and `wait_on_events_polling`'s
own already-once-fixed deadline-ambiguity guard (`has_real_deadline`/`remaining_timeout()`,
`unix.rs:2705-2746`, itself the product of a live fix earlier this same day for an unbounded-poll-
loop bug in this exact function) both read as correct in isolation, yet the observed stall vastly
exceeds 3 seconds (100s+ real wall-clock, both runs) -- either this specific call path never
actually reaches that bounded wait (most likely, given (a) above), or there is a third, not-yet-
found bug in the same neighborhood. **Concrete next step**: `cdb -pv` (non-invasive) attach to the
specific stuck winpid the moment the stall is confirmed (identify it via the same `task-resume-
probe (child, winpid=NNNNN)` log line this pass used) and run `~*k` -- near-zero CPU across a
multi-second sample already rules out a spin loop, so the resulting stack should show exactly which
wait primitive (a `RawMutex`, an OS-level `WaitOnAddress`/event, or something else) the thread is
genuinely blocked in, rather than further static reading of code that looks correct on paper. This
is likely the same general "cross-process AF_UNIX connection data plane" area Track B pickup (-2)
already flags as "genuinely probabilistic" for a different symptom (`net::wait_on_tun`) -- worth
checking whether this is that same probabilistic class recurring deterministically for a different
reason, or a genuinely separate, third mechanism in the same neighborhood.

**Host state**: single runner instance at a time throughout (confirmed via `Get-Process` before
every launch, and a forced `Stop-Process` between each); host free RAM fluctuated between ~1.7GB
and ~4.3GB across this pass's several launches with no litebox process running at the low points
either, consistent with every prior session's "RAM pressure is host-wide and unrelated to litebox"
finding -- proceeded per standing instruction rather than treating it as a blocker. Files touched:
`litebox_shim_linux/src/lib.rs` only (the `GlobalState`/`GlobalStateHandle` fix above); no test
files added, per standing rule. `.wfgy/cdb_catch_xset.txt` (the cdb command script) and this pass's
`.wfgy/cdb_boot_catch*.log`/`boot_memfds_fix*.log`/`boot_debug_unix.log` repro logs are gitignored
scratch artifacts, not committed.

**Addendum, same pass: a non-invasive `cdb -pv` symbol-resolved snapshot of a THIRD live
occurrence of this stall, and why it does NOT settle the question.** With `-y <target/release>`
(this build's own `.pdb` is present) and `.reload /f`, `~*k` on the stuck winpid resolved real
function names instead of raw offsets, cross-process-safely (`-pv`, confirmed `Detached` cleanly,
target still alive afterward). Five threads total: (0) `main` blocked in `Thread::join` on the
guest-execution thread (expected); (1) a guest-level pty/epoll wait inside
`diag_process_fork_globalstate_probe_inner` (a second, DIFFERENT guest thread, unrelated); (2) the
already-known `fault_terminate_watchdog_thread_body` sleep loop (expected, benign); (3) the REAL
guest-execution thread (matches this pass's own `task-resume-probe` log line, confirmed via
`diag_process_fork_task_resume_probe` in its own stack), blocked in `Condvar::wait_timeout` ->
`futex_wait` -> `WaitOnAddress`, symbol-resolved as `litebox_platform_windows_userland::net::
wait_on_tun+0xf8` called from `syscalls::process::sys_execve::copy_vector+0xd78`; (4) a separate
thread blocked in a literal `thread::sleep` inside `OnceLock::call_once_force` ->
`SharedArc::...::shared_arc_probe_parent_prepare` -> `net::NatGateway::new`, i.e. a real,
plausible retry-backoff loop lazily constructing the NAT gateway singleton. **This does NOT
confirm the fifteenth pass's `wait_on_tun`-REFUTED finding was wrong**: frame 3's own caller
offset (`copy_vector+0xd78`, +3192 bytes) is far too large to trust for a ~40-line function that
does nothing but walk pointers and build `CString`s -- checked directly against
`litebox_shim_linux/src/syscalls/process.rs:5985-6022`'s actual source, which contains no
networking call of any kind. This is exactly the symbol-resolution-noise failure mode this
project's own standing lesson already names ("trust only small offsets," `docs/
AGENTS_ARCHIVE_2026-09-17.md:1852`) -- `wait_on_tun`'s own thin wrapper around the generic
`Condvar::wait_timeout` is a strong ICF (identical-code-folding) merge candidate with some other,
differently-purposed thin wait wrapper, and the debugger has no way to disambiguate which one a
merged symbol's name actually refers to at this call site. **What IS reliable**: this process is
genuinely blocked (not spinning) inside SOME capped-or-uncapped `Condvar`-based wait reached from
somewhere inside real guest-execve-adjacent code, concurrently with a second thread genuinely
sleep-retrying `NatGateway::new`'s lazy `OnceLock` initialization -- a real, live, two-thread
wait/init relationship worth a dedicated future pass, but NOT provably the AF_UNIX
`connect_cross_process` path this pass's earlier, non-symbolized `Abstract([47, 116, 109, 112,
...])`-address evidence pointed at (a DIFFERENT stuck winpid, different occurrence -- the two may
be entirely different mechanisms that both happen to stall around the same point in the boot
script, not one mechanism). **Concrete next step, refined**: do not trust either symbol name for
the exact function without cross-referencing source the way this addendum just did for
`copy_vector`; either build a debug (non-LTO/non-ICF) binary for one targeted repro, or add a
temporary `eprintln!` directly at the real `wait_on_tun`/`NatGateway::new` call sites to prove
which (if either) genuinely fires here, before spending further time reading the release binary's
own disassembly.

## Eighteenth pass, 2026-09-18 -- systematic GlobalState field audit, debug binary, unambiguous stall read

**Eighteenth pass, 2026-09-18 -- systematic `GlobalState` field audit (3 more defects fixed), a
debug build for reliable `cdb` symbols, and an unambiguous read on the post-xset stall.**

*Audit.* Every field of `GlobalState` (`litebox_shim_linux/src/lib.rs`) was classified by hand:
POD/pointer-free (safe as-is) vs heap-indirection (BTreeMap/Vec/Arc, needing per-process shadowing
or a real shared-arena-native redesign, per which semantics it actually needs). Three more
defects found, not yet fixed by any prior pass, all fixed and verified this pass (66 `litebox`
unit tests pass, cargo check clean, cheap repro passes):
- `unix_addr_table` (BTreeMap) -- its own companion table's doc comment already said entries are
  kept "alongside (never instead of) each process's own real `UnixAddrTable`", i.e. always meant
  to be per-process-private. Shadowed onto `GlobalStateHandle`, same treatment as
  `elf_patch_cache`.
- `fifo_registry` (BTreeMap) -- its own doc comment already said "shared by every thread of this
  process -- but NOT across processes". Same shadow fix.
- `sysv_shm` (two BTreeMaps) -- genuinely DOES need real cross-process visibility (X11 MIT-SHM /
  Xvfb's `-shmem` framebuffer / selkies pixelflux all depend on two different guest processes'
  `shmget(same key)` resolving to the same segment), so shadowing would break real semantics.
  Redesigned as a fixed 128-slot pointer-free array (`SysvShmSegment` is already fully
  `Copy`/POD) -- inherits `GlobalState`'s own cross-process sharing for free, same pattern
  `SharedUnixAddrPresenceTable` established.

Remaining fields the same audit found still defective, NOT yet fixed (deeper redesign than a flat
Copy-slot array, since their payload types own real heap state -- `Arc<FlockFile>`/`PtyFd`
ring-buffers, `Pollee` observer lists): `pty_registry`, `daemon_pty_masters`, `flock_registry`,
`drm` (`DrmSubsystem`), `evdev` (`EvdevSubsystem`) -- all genuinely need cross-process visibility
per their own doc comments (a real global `/dev/pts`/flock/DRM/evdev namespace), none touched by
the Xvfb/selkies boot path this investigation targets, so left as follow-on work (pickup list
below). `bootstrap_process` (`OnceBox<Arc<Process>>`) was also audited and is DIFFERENT in kind
from the rest: it is set exactly ONCE, before any fork ever occurs, so on this codebase's real-
address-space-duplicating fork model every later descendant should have a valid mapping at that
address by construction (unlike a `BTreeMap` mutated post-fork by many different processes) --
plausibly already safe; left as-is pending live confirmation rather than patched speculatively.

*Debug binary.* `cargo build -p litebox_runner_linux_on_windows_userland` (no `--release`) --
the workspace has no `[profile.release]` override anywhere, so "release" is plain
`opt-level=3`+default codegen-units, and the ambiguous/merged symbols both this pass and the
seventeenth pass hit are MSVC linker-level ICF (identical-code-folding, `/OPT:ICF`, applied by
default once optimization is on) folding distinct functions into one symbol, not LTO (already
off). The plain `cargo build` dev profile disables optimization entirely (codegen-units=256,
opt-level=0), which keeps ICF from ever triggering. Produces
`target/debug/litebox_runner_linux_on_windows_userland.exe` + matching `.pdb` (~25MB exe, ~240MB
pdb.) Confirmed live: boots the real cheap repro correctly; booted the real
`.wfgy/webtop_stack.sh` workload (much slower -- fine, diagnostic only) and reproduced the same
stall as the release binary at the same script offset. **Use this binary, not the release one,
any time a `cdb` read needs to be trusted** -- symbol path `target/debug` (matching `.pdb` sits
next to the exe already, no separate copy step needed the way the release-binary symbolizer
script requires).

*The post-xset stall -- unambiguous read obtained, BOTH prior candidate theories REFUTED, real
blocking site pinned down.* A non-invasive `cdb -pv` snapshot of the RELEASE binary stuck at the
same point reproduced the exact ambiguity the seventeenth pass flagged: one thread's stack read
as `syscalls::process::Task::sys_execve::copy_vector+0xd78` calling directly into `net::
wait_on_tun` (impossible per source, confirmed again), a different thread simultaneously showed a
`NatGateway::new` call folded into an unrelated `shared_arc_probe_parent_prepare` diagnostic
closure's name -- both textbook ICF garbling, not real call graphs. Re-ran the IDENTICAL repro
(`LITEBOX_PROCESS_FORK=1` + `.wfgy/webtop_stack.sh`, `--resume-from .wfgy/webtop_seed.tar`)
against the new DEBUG binary; it stalled at the same script offset (19289, `xrdb`'s own line,
winpid distinct each run) and a `cdb -pv -y target\debug` snapshot this time gave 7 clean,
internally-consistent, non-folded stacks (`.wfgy/cdb_debugbuild_snapshot.log`):
- Both release-build candidates are explained as false leads from otherwise-legitimate BACKGROUND
  threads, not the blocking site: one thread genuinely is inside `net::wait_on_tun` -- but it is
  the per-fork-child `net_worker` thread's OWN normal <=1ms-bounded poll loop
  (`litebox_runner_linux_on_windows_userland/src/lib.rs:1935-1966`), doing exactly what it always
  does; another genuinely is inside `NatGateway::new`'s own background retry closure, also
  ordinary standing infrastructure. Neither blocks guest forward progress.
- The REAL blocked thread (unambiguous, clean symbols, zero inlining/folding): a guest `ppoll()`
  syscall -- `litebox_shim_linux::syscalls::file::sys_ppoll` -> `epoll::PollSet::wait` ->
  `litebox::event::wait::WaitContext::commit_wait` -> `RawMutex::block_or_maybe_timeout` --
  genuinely parked on a `Condvar`. `PollSet::wait` (`syscalls/epoll.rs:929`) already contains the
  seventeenth-pass AF_UNIX bounded-15ms-repoll fix (`has_unwakeable_fd` matches
  `EpollDescriptor::Unix`), so this is NOT the "no wake at all" gap that fix closed -- the log's
  own evidence (`self_pid=13684 owner_pid=13008`, a `[unix_addr_presence] ECONNREFUSED... bound by
  a DIFFERENT guest pid` WARN printed 3.4s into this run, before the stall) shows this same guest
  process already took the `connect_cross_process` cross-process path once. The bounded repoll IS
  running (small-but-nonzero CPU matches a low-duty-cycle 15ms loop, not a true freeze) but
  whatever readiness condition it is polling for (`Backlog::check_io_events`'s
  `unix_shared_connect_queue.has_pending(...)` check, `syscalls/unix.rs:449-467`, looked correct
  on inspection) never flips true. **Not yet fully root-caused**: did not get far enough this pass
  to pin down, with live variable inspection (`cdb`'s `dv`/`dt` against this exact stuck thread),
  which specific fd/direction is polling (listener-side `Backlog::check_io_events` vs. an
  already-`connect()`ed client waiting on a reply that its peer never sends) -- next session:
  reproduce again, and BEFORE anything else `dt`/`dv` thread 1's `PollSet` locals (entries/fds) at
  the exact stuck point to identify the fd, then trace which side of the rendezvous never posts.

## Nineteenth pass, 2026-09-18 -- live cdb repro of the post-xset ppoll stall (twice), has_pending REFUTED as the bug, real client-side gap found, next-step evidence still missing

**Setup.** `LITEBOX_PROCESS_FORK=1` + debug binary (`target/debug/litebox_runner_linux_on_windows_userland.exe`,
built earlier today, matching `.pdb` present) + `.wfgy/webtop_stack.sh` via
`--resume-from .wfgy/webtop_seed.tar` (regenerated same day, newer than the script, confirmed
current). Launch script: `.wfgy/repro_debug_ppoll_stall.ps1` (same shape as `.wfgy/ab_repro_new.ps1`,
pointed at the debug binary and port 8090). `cdb.exe` at
`C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe`; every attach used `-pv` (non-invasive)
with `-y target\debug` for symbols, never a bare `q` to detach.

**First repro.** Boot reached `NGINX_STARTED`/`NGINX_SELFTEST_FAILED` (expected, open/tracked
separately), then went fully silent (no log growth) for 60+ seconds -- past the script's own bounded
60s `[ -e "$XSOCK" ]` wait loop, so this was a genuine stall, not that loop's own silence. A full
`cdb -pv -p <pid> -y target\debug -c "~*kb;qd"` sweep of every live `litebox_runner_linux_on_windows_userland.exe`
process found exactly one (winpid 17248) with a `sys_ppoll -> PollSet::wait -> commit_wait ->
RawMutex::block_or_maybe_timeout` frame -- an exact match for the eighteenth pass's own read.
Decoded boot log correlation (`iconv -f UTF-16LE -t UTF-8`, PowerShell `*>` redirection is UTF-16):
`task-resume-probe (child): guest fd 255 reopened on /webtop_stack.sh at offset 19289` then
`task-resume-probe (child, winpid=17248): built Task, ... entering real guest execution` then a
`3.412319000s WARN litebox_shim_linux::syscalls::unix: [unix_addr_presence] ECONNREFUSED but
address IS bound, by a DIFFERENT guest pid ... listener self_pid=17248 owner_pid=9964` then
`/webtop_stack.sh: line 290: 149 Killed xset q > /dev/null 2>&1` then `[s] XVFB_FAILED`.

Script offset 19289 is `xset q`'s own line (confirmed by direct `sed -n` on `.wfgy/webtop_stack.sh`),
not `xrdb`'s (the eighteenth pass misread this by one command -- `xrdb` is the very next line,
offset ~19468, and DOES run, harmlessly failing with `.Xresources: No such file`).

**Identity of `self_pid`/`owner_pid`: confirmed to be the real Windows PID, not a small guest-internal
counter.** `task.pid.get()` literally equals the host `winpid` for a `LITEBOX_PROCESS_FORK=1` child --
proven by grepping the decoded log for `winpid=9964` and finding its own `task-resume-probe` resume
at script offset 16877, which is `/usr/bin/Xvfb "$DISPLAY" ... &`'s own line. So `owner_pid=9964`
in the WARN directly names Xvfb's real host process, and `self_pid=17248`/`self_pid=4700` (see
below) directly name xset's.

**Chasing "Killed" as a literal SIGKILL was a dead end.** Windows PID 17248 was found ALIVE (via a
second, later `cdb -pv` attach) still blocked in the exact same `ppoll` stack MINUTES after its own
"Killed" log line -- initially read as "the guest was told this process died but the real OS process
never actually terminated" (a scary, novel bug class). This is very likely WRONG: `Get-Process -Id
17248`'s own `StartTime` (10:30:58) postdated the boot log's own "Killed" event by roughly ten
minutes of wall clock, and dozens of short-lived children (nginx-supervisor retries) cycled through
PIDs in between -- ordinary Windows PID reuse is the far more likely explanation than a zombie
surviving its own reported death. Do not re-chase this specific "SIGKILL but still alive" angle
without first correlating `Get-Process`'s own `StartTime` against the exact boot-log timestamp for
the SAME winpid, in the SAME run, to rule out reuse definitively either way.

**`decode_cross_process_wait_status`/`CROSS_PROCESS_EXIT_MARKER` (`syscalls/process.rs:334-403`)
investigated as a candidate universal bug, then ruled out as the live cause here.** The function's
own doc comment says the encode-side call site "does not exist yet" and it carries
`#[allow(dead_code)]` -- true of the "production" `do_clone`-driven path. But the ACTUAL mechanism
driving every `LITEBOX_PROCESS_FORK=1` child in this whole day's testing is a diagnostic/resume
harness in the runner crate (`diag_process_fork_task_resume_probe` et al., visible on every stuck
thread's own call stack, well below `run_thread_with_fork_verification`) which DOES correctly print
`exiting with encoded status 0xc0de0000` (marker + exit code 0) for every child that reaches its own
normal exit -- confirmed by grepping dozens of such lines in the decoded log, all for children other
than xset. xset's own resume block has NO such line before "Killed", meaning it never reached its
own exit path at all -- consistent with "genuinely still executing (blocked) when something else
killed it", not "a normal exit misreported as a kill". This whole angle is a real, confirmed,
still-relevant piece of dead code (worth fixing eventually so a FUTURE externally-killed child's real
Linux exit code isn't lost), but not the live blocker here.

**Second, independent repro (retry of the same script line, later child).** After `XVFB_FAILED`, the
script proceeds (xrdb fails harmlessly, the runner's own logic then triggers `[process_fork_diag]
globalstate-probe (child): rebuilding rootfs from OCI image ...` -- a full, all-cache-hit re-pull,
real wall-clock cost even cached) and resumes at offset 19468 as winpid 4700, which hits the
IDENTICAL WARN (`self_pid=4700 owner_pid=9964`) and then goes silent forever -- no `[s]` marker, no
further `task-resume-probe` line, ever, for the rest of the observation window (20+ minutes). A fresh
full `cdb -pv` sweep caught winpid 4700 in the EXACT SAME `sys_ppoll -> PollSet::wait -> commit_wait`
stack as 17248 before it. This is strong evidence the stall is deterministic and tied to this exact
script step (xset's cross-process connect to Xvfb), not a one-off.

**Frame numbering, confirmed stable across both captures (for the next session's `dv`)**: `k` on the
stuck thread consistently shows: 00 `ntdll!NtWaitForSingleObject`, 01 `KERNELBASE!WaitForSingleObjectEx`,
02 `RawMutex::block_or_maybe_timeout`, 03 `impl$27::block_or_timeout`, 04 `commit_wait`, 05
`wait_until`, 06 `PollSet::wait`, 07 `sys_ppoll::closure$1`, 08 `Task::sys_ppoll`. `.frame 06; dv /t
/v` is where `self` (the `&mut PollSet`, whose `entries: Vec<PollEntry>` names every polled fd + its
requested mask) should be readable.

**Why the locals were still NOT captured this pass, despite two tries each on 17248 and on 4700.**
Every follow-up `cdb -pv` attach (issued as a SEPARATE PowerShell/cdb invocation, seconds to ~1 minute
after the sweep that found the thread) hit `Unable to examine process id <pid>, HRESULT 0x80004002`
-- the process had already exited by the time the second command ran. Both 17248 and 4700 independently
show the SAME pattern: alive and genuinely blocked in `ppoll` when swept, gone (not replaced by a
further script step) within roughly 1-3 minutes of being caught. This means the stuck process is not
eternally frozen -- something eventually reaps it -- but whatever SHOULD happen next in the script
never does (no further `[s]` marker, no further `task-resume-probe`, ever, for the remainder of every
observation window run this pass). **Concrete fix for next time**: combine discovery and locals
inspection into ONE `cdb` invocation/command string (e.g. run `k;.frame 6;dv /t /v` against every
thread of every candidate pid in the SAME dispatch as the sweep) rather than two separate `Invoke`s,
to close this race entirely.

**Static code review of the AF_UNIX rendezvous itself (`syscalls/unix.rs`): no bug found.**
`SharedUnixConnectQueue::{post, has_pending, try_claim, complete, poll_result, cancel}`
(lines ~3045-3160) read as a correct, simple atomic state machine (`REQ_EMPTY -> REQ_WRITING ->
REQ_PENDING -> REQ_CLAIMED -> REQ_ACCEPTED -> REQ_EMPTY`), matched by `(kind, key)` via
`PendingConnectRequest::matches`. `presence_kind_and_bytes`/`UnixSocketAddr::to_key`/
`UnixBoundSocketAddr::to_key` are the SAME functions used on both the `listen()`-side insert
(`unix.rs:263-270`) and the `connect_cross_process`-side lookup/post (`unix.rs:1360-1376`), so a
key-encoding mismatch between listener and client is structurally ruled out, not just unobserved.
`Backlog::check_io_events` (`unix.rs:449-467`) correctly falls through to `has_pending` only when the
private same-process backlog is empty and not shut down, and `UnixStream::check_io_events`
(`unix.rs:1590-1606`) correctly routes `Listen` state to it with a captured `GlobalStateHandle`
(`listen.global`, set at `listen()` time) -- no missing-`global`-parameter gap either.
`PollSet::wait`'s `has_unwakeable_fd` check (`epoll.rs:959-977`) correctly matches
`EpollDescriptor::Unix(_)` unconditionally (any unix-socket fd, not just listeners), so the
bounded-15ms-repoll path is taken and IS running for this fd, exactly as the eighteenth pass found.

**Real gap found instead, NOT the confirmed live cause but a genuine bug on its own merits, NOT
fixed this pass.** `wait_on_events_polling` (`unix.rs:2705-2746`, used by `connect_cross_process`'s
own bounded wait) delegates to `litebox::event::polling::WaitContext::wait_on_events`
(`litebox/src/event/polling.rs:49-84`), whose very first lines are `match try_op() { Err(TryOpError::
TryAgain) if !nonblock => {} ret => return ret }`. For a NON-BLOCKING `connect()` (`nonblock ==
true`), this returns immediately -- a single non-waiting check, no observer registration, no loop --
the moment the connection hasn't ALREADY completed synchronously. `connect_cross_process`
(`unix.rs:1354-1424`) posts into `unix_shared_connect_queue` unconditionally before this check, so
the request DOES sit in the queue and the listener's `accept()` loop WILL eventually claim+complete
it -- but the client's OWN `request_idx` is a plain local variable, stored nowhere on
`self`/`UnixInitStream`, and is simply dropped once `connect_cross_process` returns
`EINPROGRESS`-equivalent to the guest. `UnixInitStream::check_io_events`, reached via
`UnixStream::check_io_events`'s `Init` arm (`unix.rs:1590-1602`), is a STATIC report (`OUT|HUP`,
plus `IN` only if `read_shutdown`) that never touches `unix_shared_connect_queue` at all. So a
subsequent `poll()`/`select()`/`ppoll()` on that same fd -- the normal POSIX pattern for a
non-blocking connect (`connect()` once, then `poll()` for `POLLOUT`) -- can NEVER observe the
connection actually completing; the socket is stuck reporting `Init`'s state forever, even once the
real cross-process connection is sitting fully established in `unix_shared_conn_table`, unclaimed by
anyone. **This is real and confirmed by code reading alone**, but NOT yet confirmed as xset's own
specific mechanism: xset is old, simple Xlib-based `x11-utils` code, and Xlib's own connect path is
BLOCKING by default, so this exact gap more plausibly explains a LATER, more modern, async-socket-
based client (dbus client libraries, GTK/XFCE session components using non-blocking connects) than
`xset` itself. Deliberately NOT fixed this pass -- the task's own stated methodology ("`dt`/`dv` the
stuck thread's `PollSet` locals FIRST to identify the exact fd/direction before touching any code")
was followed in spirit: the locals were sought repeatedly and genuinely, but the race described
above prevented capturing them, and patching this specific gap without knowing whether it's even the
right fd would be exactly the "patch blind" this project's own standing rules warn against elsewhere
(`litebox/src/event/wait.rs:224`'s `unreachable!()`, same principle).

**Ruled out, not a bug**: process `13448` (persistent since early boot, `StartTime` unchanged across
many samples) is the `nginx_supervisor.sh` loop, legitimately blocked in `sys_wait4`'s "any child"
path waiting on its own currently-alive supervised nginx child -- confirmed via its OWN full `cdb
-pv -c "~*kb;qd"` thread dump (5 threads: main joining the guest thread, the `sys_wait4` blocker, a
`net::wait_on_tun` background poll thread matching AGENTS.md's own established "innocent background
infrastructure" finding, and two more sleep-loop watchdog threads). Processes `3220`/`15140` are
`run_external_fault_watchdog_child` -- single-thread host-side crash watchdogs, not guest execution
at all, not relevant to this investigation.

**Open question flagged for next session, not chased further this pass due to time**: once a `ppoll`-
stuck child IS eventually reaped (confirmed it does happen, just later than expected), NOTHING
continues the boot script afterward in any run observed this pass -- log stays frozen indefinitely
past that point, no new `task-resume-probe`/`[s]` line ever appears again. This could be (a) a
genuinely separate bug in the resume/continuation chain that drops the next script step specifically
when a child is externally-timed-out rather than exiting via its own normal path, or (b) simply that
NOTHING in this specific script continues past a failed `xset q` besides `xrdb`+`dbus-launch`, and
one of THOSE is itself independently stuck on the exact same non-blocking-connect gap described
above (dbus client libraries are prime non-blocking-connect suspects) -- these two explanations are
not mutually exclusive and either would look identical from the outside (silence forever). Next
session's very first move should be exactly what this pass's own methodology called for and could
not quite land: catch a fresh stuck thread and read `PollSet::wait`'s `entries` in the SAME cdb
dispatch that found it, before it can self-terminate out from under a second attach.

**Live evidence artifacts this pass** (gitignored, `.wfgy/`, not committed): `.wfgy/
repro_debug_ppoll_stall.ps1` (ready-to-rerun launch script), `.wfgy/debug_ppoll_stall_boot.log`
(UTF-16, decode with `iconv -f UTF-16LE -t UTF-8`), `.wfgy/cdb_17248.log`/`cdb_4700.log` (initial
sweeps showing the stuck stack), `.wfgy/cdb4_13448.log` (nginx-supervisor full dump ruling it out).

**Host state at end of pass**: all `litebox_runner_linux_on_windows_userland.exe` processes started
this pass were killed (`Stop-Process -Force`); host free memory ~6.35 GB of ~15.2 GB total,
consistent with the ~6.5 GB free measured at the start of this pass (no leak from this session's own
activity).

## Twentieth pass, 2026-09-18 -- root-caused and FIXED the SharedUnixConnTable slot leak; found (not yet fixed) a second, separate wait4(-1) stall

Booted `.wfgy/webtop_stack.sh` under the debug binary + `LITEBOX_PROCESS_FORK=1` per
`.wfgy/repro_debug_ppoll_stall.ps1`. Fixed the cdb symbol-loading problem that blocked the
nineteenth pass's `dv`/`dx` locals dump: `-y target\debug` gets mangled by Git Bash's automatic
path conversion (backslashes silently dropped, `target\debug` becomes `targetdebug`, "system cannot
find the file specified"). Fix: set `_NT_SYMBOL_PATH` as an environment variable
(`export _NT_SYMBOL_PATH='C:\dev\litebox-main\target\debug'` -- backslashes survive fine as a plain
env-var value, unlike a `-y` command-line argument) instead of passing `-y`/`.sympath` on the
command line; also `export MSYS_NO_PATHCONV=1`. Also confirmed cdb (this build, 10.0.18362.1) only
honors the LAST `-c` flag if given multiple -- chain everything into one `-c "cmd1;cmd2;..."`
string instead. `~*e "cmd"` (broadcast a command to every thread) silently produced zero output in
every trial here (cause not fully isolated); the reliable alternative that DID work: explicit
per-thread `~Ns;.echo THREADN;.frame 6;dv /t /v` chains, N = 0..7 (a thread index past the
process's actual thread count just errors "Illegal thread error" harmlessly and the chain
continues).

**Live decisive evidence obtained** (single `cdb -pv` attach, pid 17296, a selkies-related
cross-process-forked child caught stuck in the exact `sys_ppoll -> PollSet::wait -> commit_wait ->
RawMutex::block_or_maybe_timeout` stack, thread 1, frame 6):
```
self = 0x...273f3cb0  (PollSet<WindowsUserland>*)
has_unwakeable_fd = true
register = false
```
**This refutes, with direct live evidence, any hypothesis that PollSet::wait's AF_UNIX
bounded-15ms-repoll path (`epoll.rs`, `has_unwakeable_fd`/`STDIN_REPOLL_INTERVAL`, landed 13th
pass, commit `b86f1f1`) is inactive or broken for a Shared-transport AF_UNIX fd.** It is correctly
engaged (`has_unwakeable_fd=true`) and has already cycled through at least one bounded
wait-then-rescan iteration (`register=false`, which the code only sets on a repoll-loop iteration
AFTER the first). A thread caught in this exact stack is NOT permanently blocked on an
un-signalable condvar -- it is actively, correctly re-scanning `check_io_events`/
`check_io_events_shared()` every ~15ms. The only way this can still appear stuck for minutes is if
`check_io_events_shared()` genuinely, persistently never observes readiness -- i.e. the PEER
(usually Xvfb) never actually writes the expected reply into the shared ring at all.

**Code review of the write/notify path (`syscalls/unix.rs`) confirms this is architecturally
expected, not a bug in itself**: `try_sendto_shared`/`SharedByteRing::try_write{_all}` write real
bytes into real shared-arena memory (visible cross-process, correctly paired via `shared_rings`'s
`is_client` swap -- no read/write-ring mixup found), but call `self.pollee.notify_observers(...)`
NEVER on the write path (only `try_recvfrom_shared` does, to unblock a local blocked WRITER, which
is same-process-only anyway) -- there genuinely is no push-based cross-process wake for this
transport, which is why the bounded-repoll design exists at all (module doc comment: "No genuine
cross-process wakeup... driven by call sites... re-polling on a short bounded timeout"). Confirmed
this is BY DESIGN and already correctly engaged. The open question is therefore squarely on the
SERVER (accept/reply) side, not the client's poll -- not fully resolved this pass (a second catch,
pid 9192, missed: thread numbering is NOT stable across different guest binaries -- `xset`/`xrdb`
had the ppoll thread at index 1 twice; a Python/selkies-class process's thread layout differs and
the same `~1s` guess landed on a thread-pool worker instead, "Cannot find frame 0x6" -- future
attempts must dump `~*kb` first and search its OWN output for the matching stack shape per-process,
never assume a fixed thread index across different guest binaries).

**Root-caused and FIXED, this pass: SharedUnixConnTable's fixed 8-slot pool permanently leaks one
slot per stuck client killed externally.** `SharedConnSlot::free()` (`unix.rs` ~2983) is only ever
called from `ConnTransport::Shared`'s `Drop` impl (~line 685/709) -- and `Drop` NEVER runs when a
process is torn down by `TerminateProcess`/WMI `Terminate` (the very kill mechanism this whole
investigation's own timeout-based catch-and-kill relies on). Traced this live across the session:
at least four independent cross-process children (xrdb-class retries at winpid 13888/16592,
selkies-class retries at winpid 17296/9192) were each caught stuck in exactly this `sys_ppoll` stack
and subsequently disappeared without ever completing their X11 round trip -- each one leaked its
`unix_shared_conn_table` slot. Direct log evidence of the compounding effect: a `curl`-based
`SELKIES_PORT_UP` readiness-poll loop (`webtop_stack.sh` offset ~45304) that should complete in a
handful of one-second iterations instead spawned 40+ distinct forked `curl` child winpids in rapid
succession -- consistent with every fresh selkies (re)connect attempt racing an
already-shrunk-or-exhausted slot pool and failing fast (`ECONNREFUSED` via the 3-second
`SHARED_UNIX_CROSS_CONNECT_TIMEOUT` bound) rather than the boot ever reaching `SELKIES_PORT_UP`.

**Fix landed** (commit `05d279d`): `litebox::platform::SystemInfoProvider` gained
`is_process_alive(pid) -> bool` (default `true`, i.e. "assume alive, never reclaim" for any
platform that doesn't override it). `WindowsUserland`'s override reuses the exact
`OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `GetExitCodeProcess` pattern
`RawMutex::try_recover_from_dead_holder_unregistered` already established for the identical
dead-holder-recovery problem on a different shared primitive (not factored into a shared helper
this pass). `litebox::LiteBox<Platform>` gained a `pub fn platform(&self) -> &'static Platform`
accessor so `litebox_shim_linux`'s `SharedUnixConnTable` can reach it via
`global.litebox.platform()`. `SharedUnixConnTable::alloc` now falls back to a reclaim pass when no
`EMPTY` slot is found: for each `OCCUPIED` slot, if BOTH the recorded `client_pid` AND `server_pid`
are confirmed dead, CAS-reclaim it and retry the claim. Deliberately conservative -- requiring BOTH
endpoints dead means a slot whose long-lived server (e.g. Xvfb, which stays alive for the whole boot
and has no signal that its peer died) never actually gets reclaimed this way;
`SHARED_UNIX_CONN_CAPACITY` was therefore also raised 8 -> 64 (still tiny, ~256 KiB total) purely to
buy more headroom against the still-not-fully-reclaimed Xvfb-survives case, pending a real fix for
that remaining half (a genuine candidate: have the SERVER side notice a peer whose owning process is
confirmed dead and self-shutdown its own slot half -- not attempted this pass).

Rebuilt cleanly (`cargo build -p litebox_runner_linux_on_windows_userland`, no errors, one
pre-existing unrelated `dead_code` warning). Re-booted to verify: this run did NOT reach the
AF_UNIX/Xvfb section again before running into a SEPARATE, not-previously-investigated stall (see
next section) -- so the `SharedUnixConnTable` fix's live end-to-end effect on reaching a working
desktop is UNVERIFIED this pass (verified by code review + compilation only).

**New finding, NOT yet fixed: the root script interpreter's own `wait4(-1)` can hang forever, with
NO bounded-repoll fallback, even after the cross-process child it's waiting for already exited
CLEANLY (not killed).** Live `cdb -pv` on the persistent root process during the post-fix
verification boot showed thread 1 parked in `litebox_shim_linux::Task::sys_wait4 -> ... ->
WaitContext::wait_until<...,syscalls::process::impl$10::sys_wait4::closure_env$0<...>> ->
commit_wait -> RawMutex::block_or_maybe_timeout`, while the log showed its most recent forked child
(winpid 7960) had ALREADY logged `task-resume-probe (child): exiting with encoded status
0xc0de0000` -- a completely normal, cooperative exit, not an external kill. `sys_wait4`'s
`pid == -1` branch (`syscalls/process.rs` ~2313-2380) blocks via a PLAIN
`self.wait_cx().wait_until(&mut poll_once)` -- unlike every AF_UNIX call site, this has NO
bounded periodic re-poll fallback at all; it relies entirely on
`arm_cross_process_exit_notifier`/`spawn_cross_process_exit_notifier` (`process.rs` ~3243-3260)
correctly firing an interrupt. Both notifier paths look structurally correct on inspection, and this
general area already has extensive prior-pass doc-comment coverage of a related race (the
`EINTR`-vs-"child already exited" re-check a few lines above) -- but this specific manifestation (a
cross-process child that had ALREADY fully exited before the wait even started blocking) was not
chased further this pass. Not confirmed deterministic -- the FIRST boot this session (before the
`SharedUnixConnTable` fix) got much further without visibly hitting this stall, so it may be
racy/intermittent like the AF_UNIX issue. **Precise next step**: reproduce again, and as soon as the
root process's `wait4` thread is caught in this stack with a child already known-exited in the log,
get `dv`/`dx` on the notifier thread/closure state to see whether it ever ran, or ran but its
`interrupt_all_threads()` call didn't reach this specific waiting thread. A real fix by analogy with
the AF_UNIX case: wrap `sys_wait4`'s `pid == -1` blocking branch in a bounded-repoll loop too, so a
missed/lost interrupt self-heals within one short timeout instead of hanging the whole script
forever.

**Host state at end of pass**: all `litebox_runner_linux_on_windows_userland.exe`/`cdb.exe`
processes started this pass were killed; host free memory ~4.2 GB of ~15.2 GB total at end of pass
(down from ~6.2 GB at pass start -- consistent with ordinary host churn, not confirmed as a
litebox-caused leak since all litebox processes were confirmed killed before this measurement).

## Twenty-first pass, 2026-09-18 -- root-caused and FIXED the wait4(-1) no-repoll-fallback stall (commit a771692)

Took the twentieth pass's own precise pickup-list lead at face value and confirmed it structurally
before touching code: read `sys_wait4`'s `pid == -1` blocking branch
(`litebox_shim_linux/src/syscalls/process.rs`, then ~2348-2380) side by side with `PollSet::wait`
(`litebox_shim_linux/src/syscalls/epoll.rs:929-1013`), the AF_UNIX/stdin/evdev bounded-repoll
pattern already proven live (13th pass, `b86f1f1`; 18th pass, `has_unwakeable_fd` confirmation).
`PollSet::wait` detects up front whether any entry is an "unwakeable" fd kind and, if so, wraps
`wait_until` in a loop using `cx.with_timeout(STDIN_REPOLL_INTERVAL)` (15ms), re-scanning
`scan_once` on every wake -- real or timed-out -- so a lost/never-fired wake self-heals within one
short interval instead of hanging. `sys_wait4`'s `pid == -1` branch had no equivalent: plain
`self.wait_cx().wait_until(&mut poll_once)`, no deadline at all, relying entirely on
`Task::prepare_for_exit`'s `parent.interrupt_all_threads()` to wake the thread. For a
cross-process-forked child, that wake has to cross a real Windows process boundary via
`spawn_cross_process_exit_notifier`'s background waiter thread -- structurally the same class of
gap already fixed for AF_UNIX/stdin/evdev, just never given the same treatment on this call site.

**Fix** (`litebox_shim_linux/src/syscalls/process.rs`, the `else` branch inside the `pid == -1`
arm of `sys_wait4`): wrapped the blocking wait in a `loop`, calling
`self.wait_cx().with_timeout(WAIT4_REPOLL_INTERVAL).wait_until(&mut poll_once)` each iteration
(`WAIT4_REPOLL_INTERVAL` = 15ms, matching `STDIN_REPOLL_INTERVAL`). On `Ok(())`, break (a child was
found). On `Err(WaitError::TimedOut)`, `continue` -- this is always our own repoll bound, never a
genuine caller timeout, because `wait4` itself takes no timeout argument so `wait_cx()` (built via
`WaitContext::new`, no deadline) never carries a real deadline here. On `Err(WaitError::Interrupted)`,
kept the existing pre-fix logic byte-for-byte (re-poll once synchronously before surfacing `EINTR`,
per the `sleep 3 & wait` race already documented there) and `break` on success instead of falling
through the old bare `match`. No other call site or behavior changed; `no_hang`/`pid > 0` paths
untouched.

**Live verification, debug binary** (`cargo build -p litebox_runner_linux_on_windows_userland`,
`.wfgy/repro_debug_ppoll_stall.ps1`, `LITEBOX_PROCESS_FORK=1`, real `docker.io/linuxserver/
webtop:debian-xfce`): booted clean, all 17 layers cache-HIT. The exact previously-fatal sequence
(`task-resume-probe (child): exiting with encoded status 0xc0de0000` for a cross-process child)
was immediately followed by the PARENT spawning its NEXT cross-process child rather than hanging --
confirmed repeatedly, dozens of times in a row, across the whole `webtop_stack.sh` nginx-config
setup block (`mkdir`/`sed`/`cp`/`ln` etc, each its own cross-process fork under
`LITEBOX_PROCESS_FORK=1`). Counted 50+ consecutive `exiting with encoded status` lines after
`[s] NGINX_STARTED supervisor_pid=20` alone (the `NGINX_SELFTEST` retry loop,
`webtop_stack.sh:205-217`, `curl -m 3`/`sleep 1` each forking), all reaped promptly, zero stalls,
confirmed via both direct log tailing (`iconv -f UTF-16LE -t UTF-8`, the boot log's actual encoding
under PowerShell's `*>` redirection) and `Get-Process litebox_runner_linux_on_windows_userland`
process-list/CPU-time growth between checks. Stopped the debug boot manually (WMI `Terminate`, per
the spinning-allocator-safe kill method) once this was conclusively demonstrated, rather than
waiting out the debug binary's much slower per-fork OCI-rootfs-rematerialization cost (every single
forked command re-walks all 17 cached layers before executing, regardless of build profile) all the
way to a full desktop.

**Live verification, release binary**: `cargo build -p litebox_runner_linux_on_windows_userland
--release` (53.74s clean, one pre-existing unrelated `dead_code` warning on `live_pty_ids`). Fresh
boot (`.wfgy/release_boot_repro.ps1`, same image/flags) independently sustained 45+ consecutive
fork/reap cycles past `NGINX_STARTED`, correctly reached the loop's own bounded, non-fatal
`[s] NGINX_SELFTEST_FAILED last_code= after 20s -- supervisor still retrying in background`
(`webtop_stack.sh:216`, this is BY DESIGN not an error -- the nginx supervisor keeps retrying in the
background and the script continues), then progressed into the Xvfb-launch section past the
self-test loop (`guest fd 255 reopened ... at offset 19217`, a script byte-offset never reached in
either boot before this pass's fix landed).

**New, smaller finding, not yet fixed**: the `[ -e "$XSOCK" ]` Xvfb-ready wait loop
(`webtop_stack.sh:274-289`) forked a fresh cross-process child at the SAME script offset (19217)
many times in a row -- consistent with its own `sleep 1` (line 288) forking every iteration, which
directly contradicts that loop's own comment (`webtop_stack.sh:268-270`) claiming `sleep`/`[` are
bash builtins on this guest ("`type sleep` on this image's bash reports 'sleep is a shell
builtin'"). Not root-caused this pass (could be a different guest shell selecting this script,
`sh` vs `bash`, or the claim could be stale/wrong) -- if confirmed, this is up to 60 avoidable
cross-process forks (the loop's own iteration cap) just waiting for Xvfb, worth a cheap dedicated
fix next pass (e.g. skip the outer loop's sleep-forking entirely by using a real builtin busy-wait
construct, or confirm+update the stale comment if `sleep` truly isn't builtin here).

**Also newly observed, non-fatal, not yet root-caused**: `webtop_stack.sh:107-110`'s
`mkdir -p /usr/share/selkies/web` immediately followed by `[ -f .../50x.html ] || printf ... >
.../50x.html` hit `/webtop_stack.sh: line 108: /usr/share/selkies/web/50x.html: No such file or
directory` on the redirect -- i.e. the directory `mkdir -p` just created one line earlier was not
visible to the very next shell operation. Same class as the already-tracked `/tmp/empty`
writable-layer cross-child-visibility gap (Track B pickup list item 3), just a different path;
purely cosmetic (the comment right above it in the script already explains this file's own
narrow purpose -- making nginx's own 502 error page not itself 404 -- so its absence changes
nothing else). Script continues past it unconditionally either way.

**Did NOT reach the XFCE-desktop/browser/terminal/apps milestone this pass.** The release boot was
stopped mid-Xvfb-launch-section, not because of any litebox stall, but on host memory: free
physical memory was observed on a genuine FALLING TREND (1.85 GB -> 1.66 GB free, of 15.6 GB total)
while the current in-flight fork's CPU time had nearly flatlined between two checks roughly a
minute apart -- exactly the documented "watch `FreePhysicalMemory`, kill on a falling trend not a
fixed RSS number" signal from this file's own standing lessons. This host had several other large
processes resident at the time (two `claude` processes, `firefox`, two `chrome` instances,
`Discord`, `MsMpEng`) totaling well over half of RAM before litebox's own ~2.5 GB across three
processes -- consistent with ordinary host churn rather than a litebox-specific leak, but the
falling trend combined with stalled fork progress was reason enough to stop rather than push
further and risk host instability. Killed all `litebox_runner_linux_on_windows_userland.exe`
processes via WMI `Terminate` (spinning-allocator-safe method); free memory recovered to ~3.6-3.7 GB
within about a second of the kill completing, confirming the drop tracked the boot's own resident
set/cache pressure rather than a runaway host-wide leak.

**Precise next step**: re-run the full release boot (`.wfgy/release_boot_repro.ps1`) with more host
RAM headroom free (close unrelated apps first, or wait for a quieter host moment), and this time let
it run all the way through `XVFB_UP`/`DBUS_UP`/`SELKIES_PORT_UP`/`DE_LAUNCHED` uninterrupted now
that both the `SharedUnixConnTable` slot leak (20th pass) and the `wait4(-1)` no-repoll-fallback
stall (this pass) are fixed. If a genuinely new stall appears, use the corrected single-attach
`cdb -pv` technique (set `_NT_SYMBOL_PATH` env var, not `-y`; chain one `;`-joined `-c` string;
dump `~*kb` before `.frame`/`dv` since thread index is not stable across guest binaries) to
diagnose it live rather than re-guessing from log inspection alone. Once a boot reaches `DE_LAUNCHED`/
`DE_UP`, connect a real browser (`chrome-devtools`/`claude-in-chrome` MCP tooling) to the selkies
port, take real screenshots, open the Applications menu, launch Terminal Emulator and confirm a
real shell prompt, and try at least one other app (Thunar/file manager) -- the standing "all apps
must work" bar this whole investigation has been aimed at.

**Host state at end of pass**: all `litebox_runner_linux_on_windows_userland.exe` processes started
this pass (both debug and release boots) were killed via WMI `Terminate` before this pass ended; no
`litebox_runner` process remained. Free physical memory recovered to ~3.6 GB of ~15.6 GB total
after the final kill (was as low as ~1.66 GB mid-boot, on the falling trend that triggered the
stop) -- consistent with the drop having tracked this pass's own boot activity, not a persistent
host-wide leak outliving the killed processes.

## Twenty-first pass (full detail, moved from AGENTS.md when it crossed 30KB)

Root-caused and FIXED the `wait4(-1)` stall (commit `a771692`): `sys_wait4`'s `pid == -1`
blocking branch had no bounded-repoll fallback (unlike every AF_UNIX/stdin/evdev call site),
relying solely on a cross-process notify that can be lost. Fixed by matching `PollSet::wait`'s
15ms bounded-repoll pattern. Debug binary sustained 50+, release 45+ consecutive fork/reap
cycles through the previously-permanent-hang path, into the Xvfb-launch section, then stopped
mid-section on a host-memory falling-trend (not a litebox hang; recovered immediately on kill)
-- browser milestone not reached.

## Twenty-second pass (full detail, moved from AGENTS.md when it crossed 30KB)

The `sleep 1`-forks-every-iteration lead CONFIRMED and FIXED (script-only, `.wfgy/
webtop_stack.sh`, gitignored, not git-tracked): live-tested in a minimal `debian:stable-slim`
container under `LITEBOX_PROCESS_FORK=1`, `type sleep` reports `sleep is /usr/bin/sleep` (NOT a
builtin -- `test`/`[`/`kill` really are, contradicting the script's own stale comment), and 3
loop iterations of `sleep 1` produced exactly 3 `[process_fork_diag] globalstate-probe (child)`
forks. Added `_nofork_tick()` (pure `SECONDS`/`[`/`:` busy-wait, zero forks, same 1-tick
granularity) and applied it to the two PURE poll loops where sleep was the only forking cost
(Xvfb `$XSOCK` wait, dbus `/tmp/addr` wait) -- left the nginx-selftest/`SELKIES_PORT_UP` loops
alone since `curl` forks there regardless, so sleep wasn't the marginal cost. Verified live:
identical 3x1s timing, zero fork children. `.wfgy/webtop_seed.tar` regenerated from the fixed
script. (Twenty-third pass found this fix itself only worked under bash, not this image's real
`/bin/dash` -- see AGENTS.md.)

Re-ran the full release-binary boot with the fix (`LITEBOX_PROCESS_FORK=1`, `docker.io/
linuxserver/webtop:debian-xfce`, port 8090:3000). Host RAM started tight (~3.5GB free of 15.6GB
total, other host apps -- not this pass's problem) and fell to as low as 0.57GB free mid-run
before recovering on its own to 2.5GB+ (no litebox action taken at that exact moment) -- noted
honestly per standing practice, this crossed into genuinely critical territory for ~15-30s,
closer to the edge than any prior pass's recorded dip. Script reached `NGINX_STARTED`/
`NGINX_SELFTEST_FAILED` (expected, by-design) and progressed to script byte-offset 20498 --
further than the twenty-first pass's best (19217), inside/just past the now-fixed `$XSOCK`/dbus
wait loops, with NO repeated-identical-offset fork storm this time (the sleep-fork fix's
intended effect, confirmed). Then genuinely HUNG: zero log growth and near-zero CPU growth on
the leaf fork child (winpid 19600) for 4+ minutes straight, no new fork children spawned.

Live `cdb -pv` on the hung leaf (release binary, `.wfgy/cdb_stall_19600.log`) found the same
5-thread shape the 18th pass flagged ICF-suspect, including a `thread::sleep` frame named
`net::NatGateway::new`. Checked that name against its actual source instead of trusting it
(`net.rs:888-926`, `lib.rs:11338-11362`): neither `NatGateway::new` nor the nearby `shared_arc_
probe` `OnceLock` init contains any retry/backoff sleep at all -- REFUTED, ICF noise, same
failure mode the 18th-pass addendum already warned about for a different frame. Rebuilt the
debug (non-LTO/non-ICF) binary and reproduced the identical stall at the identical offset
(20498); `cdb -pv` against its matching `.pdb` (`.wfgy/cdb_debug_stall_17860.log`) resolved
every frame for real this time: `wait_on_tun` and the NAT-gateway's own 5ms idle sleep are both
genuine, benign, NOT the blocker. The actual blocked thread is the fork child's real
guest-execution thread, inside a genuine guest `ppoll()` (`sys_ppoll -> PollSet::wait ->
commit_wait -> RawMutex::block_or_maybe_timeout`) right after the `ECONNREFUSED ... owner_pid=
<other child>` WARN. Killed both boots cleanly (WMI `Terminate`, RAM recovered to 4.6-4.7GB each
time). Did NOT reach the XFCE-desktop/browser/terminal/apps milestone this pass -- blocked by
this AF_UNIX/dbus `ppoll` gap, not by RAM, not by the sleep-fork issue (fixed and confirmed
working this same pass).

## Twenty-third pass (full detail, moved from AGENTS.md when it crossed 30KB)

Root-caused and FIXED a real regression in the twenty-second pass's own `_nofork_tick` fix: this
image's real `/bin/sh` is `/bin/dash` (confirmed live: `readlink -f /bin/sh` -> `/bin/dash`),
where `$SECONDS` (bash/ksh-only) is simply unset -- directly probed live: `SECONDS_IS:[]`, and
`SECONDS_AFTER:[0]` even after a real `sleep 1` (dash never auto-increments it). Before this fix,
`_nofork_tick`'s `while [ "$SECONDS" -lt "$until" ]; do :; done` ran as `[ "" -lt "$until" ]`
under dash, erroring ("Illegal number", live-caught in `.wfgy/webtop_stack.sh:82`) and exiting
non-zero, collapsing the `while` to a silent, instant no-op on its first pass -- not slow, gone:
every retry loop using it (Xvfb's `$XSOCK` wait, dbus's `/tmp/addr` wait) burned all 15 retries in
milliseconds and declared XVFB_FAILED/DBUS_FAILED before Xvfb/dbus-daemon had any real time to
start. Fixed: `_nofork_tick` now branches on `[ -n "$BASH_VERSION" ]` (unset in dash, set in
bash) -- the real busy-wait only under a shell that actually has `$SECONDS`, an ordinary forking
`sleep` otherwise (correctness over the fork-avoidance optimization). Script-only,
`.wfgy/webtop_stack.sh` (gitignored, not git-tracked), `.wfgy/webtop_seed.tar` regenerated.
Live-verified in isolation (`_nofork_tick 2` under the real `/bin/dash` now correctly takes
`ELAPSED:2` real seconds, forking `sleep` as expected) and via a full real-image boot: no more
premature XVFB_FAILED/DBUS_FAILED, confirmed by their total ABSENCE from the log this pass
(previously the very first thing printed after Xvfb starts).

With that fixed, the full boot (debug binary, `LITEBOX_PROCESS_FORK=1`) advances into the SAME
already-tracked AF_UNIX rendezvous gap, now a genuine multi-minute CPU-active livelock (not a
silent deadlock) -- confirmed via live `cdb -pv` debug-symbol frame walks on TWO independent
guest threads in TWO different host processes at once: one blocked in `sys_epoll_pwait ->
EpollFile::wait -> ... -> RawMutex::block_or_maybe_timeout` (`epoll.rs:306-378`; locals:
`has_bounded_repoll_interest=true`, `diag_iteration=0x5799`=22425), the other in `sys_ppoll ->
PollSet::wait -> ... -> RawMutex::block_or_maybe_timeout` (`epoll.rs:929-1012`; locals:
`has_unwakeable_fd=true`, `register=false`). Both AF_UNIX-aware bounded ~15ms repoll paths
(`epoll.rs:381-420`) are provably engaged and actively re-checking for 5+ real minutes straight,
never once observing readiness. Source-cross-checked both dead ends this pointed at, same
discipline as the twenty-second pass's `NatGateway::new` refutation -- neither holds up:
`EpollDescriptor::poll`'s `Unix` arm (`epoll.rs:264-267`) DOES reach the shared-queue-aware
`Backlog::check_io_events(&self, global)` (`unix.rs:452-470`) via `UnixStream::check_io_events`'s
`Listen` arm (`unix.rs:1606`, `listen.global`) -- "epoll never got the wiring" REFUTED; and
`SharedUnixConnectQueue::{post,has_pending,try_claim,complete,poll_result,cancel}`
(`unix.rs:3132-3243`) read internally consistent by inspection (matching `(kind,key)` compares,
correct `compare_exchange`-guarded state transitions) -- no obvious logic bug there either.

Genuinely NOT yet root-caused past this point (at the time): with both wait-side mechanisms
provably engaged and correctly wired to the shared-queue check, and the queue's own state machine
reading sound in isolation, the remaining gap was theorized as either (a) the connecting client's
own request being cancelled by its 3s `SHARED_UNIX_CROSS_CONNECT_TIMEOUT` (`unix.rs:2692`) before
the listener's periodic repoll ever coincides with a still-PENDING window, or (b) a
still-unidentified mismatch between the specific `Backlog` instance a listener thread is actually
polling and the specific address a client names. Both candidates REFUTED by the twenty-fourth
pass's live instrumentation below.

Both boots killed cleanly (WMI `Terminate`), RAM recovered 5-9GB free each time (host RAM was
never the constraint this pass). Did NOT reach the XFCE-desktop/browser/terminal/apps milestone
this pass -- advanced past the sleep-fork regression this pass introduced, into the same
not-yet-fully-root-caused AF_UNIX rendezvous gap the twenty-second pass already named.

## Twenty-fourth pass, 2026-09-18 -- both leading AF_UNIX hypotheses REFUTED with live per-request instrumentation; the real blocker is NOT the rendezvous mechanism; one genuine sys_wait4 bug found+fixed along the way; browser milestone still not reached

**Method.** Added real per-request diagnostic `debug!()` sites (kept, not reverted -- cheap,
gated behind `LITEBOX_LOG` module targets, matches the project's existing `TRACE unix_connect`
pattern) to `litebox_shim_linux/src/syscalls/unix.rs`: `Backlog::listen` logs `(owner_pid, kind,
key_bytes)` on every presence registration; `connect_cross_process` logs the posted request's
`(kind, key_bytes, request_idx)` and its final outcome (completed-with-slot / TIMED-OUT-and-
cancelled / other-error); `SharedUnixConnectQueue::try_claim` logs every successful claim;
`SharedUnixConnectQueue::has_pending` dumps a full snapshot of the queue's real state (every
currently-PENDING `(kind, key)`) throttled to 1-in-400 calls (~6s at the 15ms repoll cadence) per
calling process, specifically to catch a mismatch between what a listener is checking and what is
actually queued. Rebuilt the debug binary, booted twice (`.wfgy/repro_debug_unix_trace.ps1`,
`LITEBOX_PROCESS_FORK=1`, `--gui=hidden`, `docker.io/linuxserver/webtop:debian-xfce`,
`.wfgy/webtop_seed.tar`) with `LITEBOX_LOG` raised to `debug` for
`litebox_shim_linux::syscalls::unix`/`::epoll`, each run watched live 8-17 real minutes into the
same CPU-active epoll livelock previously described, RAM tracked throughout (1.4-4.8GB free,
never the constraint, recovered fully after each kill).

**Finding 1 -- the AF_UNIX rendezvous mechanism itself is sound, confirmed by direct evidence, not
inspection alone.** Across two full boot runs (~74K and ~130K raw log lines each), EXACTLY ONE
real cross-process `connect()` occurred per run -- Xvfb's own X11 socket, `kind=1`
(`presence_kind_and_bytes`'s `Abstract` tag) `key_bytes=/tmp/.X11-unix/X1` -- and it succeeded
cleanly every time, `post()` to `request completed` in 22-23ms. `Backlog::listen` fires TWICE for
this same address at Xvfb startup, `kind=1` (abstract) then `kind=0` (path) with byte-identical
`key_bytes` -- this looked like a mismatch bug at first glance but is CORRECT, standard real-Linux
X11 behavior (a real X server binds both a path AND an abstract-namespace socket for the same
display); presence lookups matched a target listener's kind+key so no client-vs-listener
byte-level mismatch was ever observed. The throttled `has_pending` queue snapshots (dozens
captured, all from Xvfb's own idle-listener checks) showed `pending_count=0` every single time
after the one real request was already claimed -- not because of a missed/mismatched request, but
because genuinely nothing else is ever queued. This directly refutes both of the twenty-third
pass's candidates: no timing race (only one request ever existed, and it landed inside its own
first ~20ms, nowhere near the 3s timeout), and no address/key mismatch (the one real request
matched on the first check).

**Finding 2 -- the real blocker is earlier, upstream of the AF_UNIX code entirely: the boot script
itself stalls in its `$XSOCK` wait loop and never reaches `xset q`.** `webtop_stack.sh:319-322`
(`while [ $i -lt 60 ]; do [ -e "$XSOCK" ] && break; i=$((i+1)); _nofork_tick 1; done`) precedes the
ONE further X11 client the boot needs (`xset q`, line 323, whose own success/failure prints
`[s] XVFB_UP`/`[s] XVFB_FAILED`). Across both full runs, this marker NEVER printed, and the
literal string `xset` never appears anywhere in either ~10-20MB trace, despite 5-17 minutes of
runtime. Also newly notable: `/usr/bin/sleep` (the dash-fallback `_nofork_tick` is supposed to
fork+exec once BASH_VERSION is confirmed unset, twenty-third pass's own fix) never appears either
-- zero occurrences in either run's full log. So the shell's own `$XSOCK` poll loop is not merely
slow; by this evidence it never completes even one full fork+exec+reap cycle of its own fallback
`sleep`. This is a DIFFERENT, upstream mechanism from anything the AF_UNIX/epoll investigation
(twelfth through twenty-third passes) was chasing.

**Finding 3 -- a genuine, live-confirmed sys_wait4 bug found and FIXED along the way, though not
sufficient on its own to unblock this stall.** `cdb -pv` on a live host process (two snapshots
20s apart, byte-identical stack) caught a thread permanently parked in `sys_wait4`'s targeted
`pid > 0` branch (`litebox_shim_linux/src/syscalls/process.rs`, the
`process.find_cross_process_child(pid)` arm) inside `RawMutex::block_or_maybe_timeout` -- this
branch called `self.global.platform.wait_for_cross_process_exit(handle)` directly, an UNBOUNDED
blocking wait with no repoll fallback at all, unlike its `pid == -1` sibling branch (already fixed
in the twenty-first pass, commit `a771692`, for exactly this same lost-cross-process-exit-notify
wake class). Fixed this pass: the `pid > 0` branch now uses the identical bounded
15ms-repoll-then-recheck pattern (`WAIT4_REPOLL_INTERVAL`, hoisted to function scope so both
branches share it), including the same `Interrupted`-before-`EINTR` re-check race handling. Real
fix, kept. Verified: a post-fix cdb re-sample of the `pid == -1` sibling branch (already fixed)
correctly showed it CYCLING (different snapshots caught different states), confirming the pattern
behaves as bounded, not stuck -- but re-running the full boot after this fix still did not reach
`xset`/`XVFB_UP` within the session's remaining time budget, so this was a real, worthwhile,
independently-justified fix, not (by itself) the fix for the `$XSOCK`-loop stall.

**Leading hypothesis for the NEXT pass, not yet directly confirmed**: the already-tracked,
still-open "writable-layer cross-child-visibility gap" (AGENTS.md pickup item 3, `/tmp/empty`)
may be the real mechanism here, at a larger scope than previously scoped. Xvfb is cross-process-
forked into its own long-running Windows process and creates `/tmp/.X11-unix/X1` (a REAL file via
`fs.open(CREAT|EXCL|RDWR,...)`, confirmed by the `kind=0` presence registration) inside ITS OWN
writable-layer view. Every "exported writable layer to ... .tar" log line observed this pass
correlates with a CHILD EXITING, i.e. writable-layer state syncs back to siblings/parent only at
exit, never continuously. Xvfb is a long-running daemon that (by design) never exits during a
normal boot -- so if this sync-only-at-exit model is exactly how litebox's cross-process fork
writable layer works, the separate shell process's `[ -e "$XSOCK" ]` check may be structurally
unable to ever observe a file Xvfb created, for as long as Xvfb keeps running (i.e. always, until
boot completes) -- a full, permanent, and previously-mis-attributed explanation for the stall,
independent of the AF_UNIX/epoll mechanism entirely. NOT yet directly confirmed with byte-level
evidence this pass (would need a live probe reading `/tmp/.X11-unix/` from both the shell's own
process and Xvfb's, or tracing the writable-layer merge/visibility code path directly) --
top-priority next step.

**Files touched this pass**: `litebox_shim_linux/src/syscalls/unix.rs` (new diagnostic `debug!()`
sites, kept), `litebox_shim_linux/src/syscalls/process.rs` (`sys_wait4`'s `pid > 0` branch given
the same bounded-repoll fallback as `pid == -1`, real fix). `.wfgy/repro_debug_unix_trace.ps1`
(new, gitignored launch script mirroring `repro_debug_ppoll_stall.ps1` with `unix`/`epoll` debug
logging raised).

**Host state**: two boots, both killed cleanly (WMI `Terminate`), zero stray
`litebox_runner`/`litebox-presenter` processes confirmed after each kill. Free RAM ranged
1.4-4.8GB across both runs (lowest point ~1.4GB during the second run's peak livelock-logging
volume, recovered to 4.5GB+ within seconds of kill) -- never the hard constraint, but closer to
the documented floor than most prior passes because the new diagnostic logging itself measurably
increases log-file I/O during the livelock window; worth keeping an eye on if a future pass
leaves this logging enabled for a long unattended run. Did NOT reach the XFCE-desktop/browser/
terminal/apps milestone this pass -- redirected the investigation away from a dead end (the
AF_UNIX rendezvous mechanism, now confirmed sound) toward the real upstream blocker (the
`$XSOCK` wait loop / writable-layer-visibility gap), fixed one real independent bug along the
way, left a precise, evidence-backed next step.

## Twenty-fifth pass -- writable-layer-visibility hypothesis CONFIRMED via source read, real
## narrow fix landed + live-verified (the `$XSOCK` stall itself is CLOSED), a second real bug
## found one step downstream and fixed, browser/terminal/apps milestone still not reached

**Method.** Read the real code, not just the prior pass's hypothesis, at every hop: `litebox_shim_
linux/src/syscalls/process.rs`'s `import_cross_process_writable_layer` (called ONLY from `sys_
wait4`'s two branches, after a child is OBSERVED TO HAVE EXITED); `litebox_runner_linux_on_
windows_userland/src/lib.rs`'s task-resume-probe tail (the child's OWN writable-layer export
happens in the last ~15 lines before `std::process::exit`, unconditionally, nowhere earlier);
`litebox_platform_windows_userland/src/process_fork.rs`'s `CONTAINER_FS_SNAPSHOT_ENV_VAR` doc
comment, which states the honest limit in so many words: "nothing propagates to an already-
running long-lived process between ITS OWN spawns." Xvfb neither exits nor spawns children on
this path, so both triggers that could ever publish its `$XSOCK` write are permanently absent for
as long as it runs -- **hypothesis CONFIRMED by direct source reading, no live dual-process probe
needed**; the design doc comment already states the exact mechanism as a known, disclosed
limitation, not a bug to be found by more instrumentation.

**Fix 1 (the real one) -- route a bound AF_UNIX path's existence through the ALREADY-shared
`SharedUnixAddrPresenceTable` instead of the general writable-layer/tar-export mechanism.** A
bound socket path is a NAME/existence marker, not real file content (litebox has no
`FileType::Socket` variant at all -- `UnixSocketAddr::bind`'s own server-side creation already
represents it as an ordinary `RegularFile`, confirmed at `litebox_shim_linux/src/syscalls/
unix.rs`), so it doesn't need the general mechanism's content-sync semantics, only a cross-
process-visible "does X exist" answer -- which `SharedUnixAddrPresenceTable` (a genuinely shared,
lock-free, fixed-slot table living in the shared kernel arena, established thirteenth pass)
already provides for exactly this purpose. Added `litebox::fs::devices::
cross_process_bound_unix_socket_status(path)` (new, `litebox/src/fs/devices.rs`, right after the
existing `devpts_*` synthetic-stat constructors it mirrors -- `FileStatus` is `#[non_exhaustive]`
so only this crate can build one) and wired it into `litebox_shim_linux/src/syscalls/file.rs`'s
`do_stat`/`do_access` as a fallback consulted ONLY on a real `ENOENT`
(`FileStatusError::PathError(PathError::NoSuchFileOrDirectory)`): if `self.global.unix_addr_
presence.lookup(UNIX_ADDR_KIND_PATH, path.as_bytes())` hits (any owner pid, including a foreign
one), synthesize the RegularFile status instead of propagating ENOENT. Logs at `warn!` (not
`debug!` -- confirmed live that `syscalls::file=debug` is FAR too hot, ~300MB/14s of pure
`sys_read` spam on this exact repro, close to making a boot untestable) exactly once per real
fallback hit, naming `path`/`owner_pid`/`self_pid`.

**Live-verified, directly, multiple independent boots (debug binary, `LITEBOX_PROCESS_FORK=1` +
`.wfgy/webtop_stack.sh`, `--oci-image docker.io/linuxserver/webtop:debian-xfce --resume-from
.wfgy/webtop_seed.tar`).** The exact log line fired as designed: `DIAG cross_process_bound_unix_
socket_stat: synthesizing stat for a sibling-bound AF_UNIX path path=/tmp/.X11-unix/X1
owner_pid=<Xvfb's guest pid> self_pid=1` -- the shell's OWN `[ -e "$XSOCK" ]` check, guest pid 1,
resolving via the presence table rather than its own private (writable-layer-blind) filesystem
view. This is the FIRST time in this entire multi-day, 25-pass investigation the `$XSOCK` wait
loop has ever broken out before its 60-iteration bound. Two independent runs (of seven total this
pass) advanced the script to offset 23017 -- PAST `xset q` (offset ~21749-21928) and into the
`dbus-launch` shim setup (webtop_stack.sh line ~343 territory) -- the furthest point any pass has
ever reached, and a strictly different, LATER blocker than anything the twelfth-through-
twenty-fourth-pass AF_UNIX/epoll investigation chain was ever chasing. **The `$XSOCK` stall itself
is CLOSED.**

**Fix 2 -- `SHARED_UNIX_CROSS_CONNECT_TIMEOUT` widened `3s` -> `15s` (`litebox_shim_linux/src/
syscalls/unix.rs`).** Once the `$XSOCK` fix let the boot reach `xset q`'s own `connect()` for the
first time ever, a SECOND, previously-unreachable bug surfaced: live `unix=debug` tracing caught
`connect_cross_process: posted request` -> `connect_cross_process: request TIMED OUT, cancelling`
for BOTH the abstract (`kind=1`) and path (`kind=0`) dual registrations, each timing out at
exactly the old `3.00s`/`3.01s` bound, even though `unix_addr_presence` confirmed the listener
(Xvfb) WAS genuinely bound the whole time. Root cause: `SHARED_UNIX_POLL_INTERVAL`'s 15ms re-poll
only fires while the LISTENER's own thread is actually scheduled, and unblocking `$XSOCK` put 8
real concurrent cross-process-forked Windows processes in flight at once (each independently
re-serving ~1GB+ of cached OCI layers on its own fork) -- host free RAM measured as low as
~300-450MB mid-boot more than once this pass, a genuine host-scheduling-latency regime the old 3s
bound was never validated against (twenty-fourth pass's own clean single-connect measurement was
~23ms, in a calm environment). Not a rendezvous-protocol defect (still confirmed sound). Widened
to `15s` -- still bounded (never the literal-forever hang the original 3s comment itself guards
against), but enough real wall-clock room for a genuinely-alive-but-starved listener to get
scheduled. **Live-verified working**: with the widened bound, a full `unix=debug` trace of a
LATER run showed the `kind=0` (path) request `posted` then, ~7s later, Xvfb's own `unix_accept`
entry, `SharedUnixConnectQueue::try_claim: claimed request ... client_pid=<xset's guest pid>`,
`unix_accept: result ok=true`, and finally `connect_cross_process: request completed ...
slot=0` on the client side -- a genuine, complete, successful cross-process AF_UNIX connection
for Xvfb's real X11 socket, the first ever directly witnessed end-to-end on this exact code path.

**Not yet closed.** (a) `[s] XVFB_UP` itself was never directly observed printing in any of the
seven boot attempts this pass -- every run that reached the connect-succeeded point was still
running (RAM healthy, Xvfb idling normally via `has_pending` polls) when this session's own
wall-clock/RAM guard killed it; the successful `request completed` trace strongly implies `xset
q` itself would go on to exit 0 and print `XVFB_UP`, but this is inference from the connect
succeeding, not a directly witnessed marker -- **top priority for the next pass: one more patient
run, watched long enough (the connect alone took ~7-19s of in-guest time this pass; budget
accordingly) to see the actual `[s] XVFB_UP`/`DBUS_UP`/`DE_LAUNCHED` markers print.** (b) The
`kind=1` (abstract-namespace) connect variant timed out even at the new 15s bound in the one run
that reached both attempts, while the `kind=0` (path) variant succeeded -- not investigated
further this pass (real X11 clients fall back path-first or abstract-first depending on library
version, so a working path-based connect is likely sufficient), but worth a dedicated look if a
future pass sees BOTH variants fail. (c) The narrow AF_UNIX-bind-path fallback does NOT fix the
general writable-layer-visibility gap for a plain file/directory a long-running sibling creates
(e.g. `webtop_stack.sh:343`'s `/tmp/empty: No such file or directory`, still observed, same
already-documented non-fatal class as before) -- deliberately out of scope (see Fix 1's own
reasoning for why the AF_UNIX case specifically doesn't need the general mechanism); the general
gap (AGENTS.md pickup item 3) remains open for anything that isn't a bound socket path.

**Host state.** Seven boot attempts total this pass, RAM fluctuated 300MB-6.9GB (host baseline
itself drifted from ~4.5GB free at session start down to ~1.6-2.9GB idle-with-zero-litebox-
processes by mid-session -- confirmed via `Get-Process | Sort WS` that this is OTHER host
software, `Resolve`/Chrome/Discord, growing over the session, not a litebox leak); every run's
process tree fully cleaned via WMI `Terminate` before the next launch, confirmed zero stray
`litebox_runner` processes after each kill. Two runs hit genuinely critical RAM (~300-450MB free,
8 concurrent processes) and were killed proactively rather than left to risk a host-wide
freeze -- both recovered fully within seconds. Did NOT reach the XFCE-desktop/browser/terminal/
apps milestone this pass, but closed the `$XSOCK` stall that has blocked every single prior pass
back to the twenty-second, found and fixed a second real bug one step downstream, and left the
boot provably closer than it has ever been (a real successful cross-process X11 socket connection,
directly witnessed).

**Files touched this pass**: `litebox/src/fs/devices.rs` (new `cross_process_bound_unix_socket_
status`), `litebox_shim_linux/src/syscalls/file.rs` (`do_stat`/`do_access` fallback wiring, new
`cross_process_bound_unix_socket_stat` helper), `litebox_shim_linux/src/syscalls/unix.rs`
(`SHARED_UNIX_CROSS_CONNECT_TIMEOUT` 3s -> 15s). `.wfgy/repro_xsock_fallback_v1.ps1` (new,
gitignored launch script, `unix=debug` logging).

### Twenty-fifth pass, continued -- the NEXT blocker found and precisely characterized:
### `xset q` writes its X11 setup request into the shared ring, but Xvfb's own read of it was
### never observed across three more full boot attempts; live cdb evidence rules out the
### obvious "connect timeout" and "genuinely frozen thread" explanations

**Method.** Added two new, permanent, low-volume `debug!()` sites (kept, cheap -- fire once per
real transfer, not per poll iteration, same discipline as the AF_UNIX rendezvous instrumentation
already in this file): `try_sendto_shared`/`try_recvfrom_shared`
(`litebox_shim_linux/src/syscalls/unix.rs`), logging `slot`/`is_client`/`len` on every real
byte-level write/read over a `Shared`-transport AF_UNIX connection -- the ONE thing the existing
`unix=debug` instrumentation never covered (it stopped at connection establishment). Rebooted with
`unix=debug` (not `epoll=debug` -- confirmed live this pass that `epoll=debug` is catastrophically
hot, 78MB of log in under 8 minutes on this exact repro, an order of magnitude worse even than the
already-documented `file=debug` cost; unconditional per-iteration `EpollFile::wait` logging is not
usable for a real boot and needs the SAME kind of throttle `has_pending`'s own 1-in-400 gate
already applies before any future pass re-enables it).

**Finding.** Across three of the run's boot attempts, `xset q`'s own initial X11
`xConnClientPrefix` write (exactly 12 bytes -- the real wire size of that struct) landed cleanly:
`DIAG try_sendto_shared: wrote slot=0 is_client=true len=12`. In every one of those three runs,
**zero corresponding `try_recvfrom_shared: read` ever appeared on Xvfb's (`is_client=false`) side**
across 8-20+ minutes of continued observation, while Xvfb's OWN listening-socket idle-accept loop
(`SharedUnixConnectQueue::has_pending`, a DIFFERENT code path) kept ticking normally on its
~3.2s cadence the entire time -- Xvfb is provably alive, scheduled, and executing, just never
observed reading the 12 bytes sitting in the ring.

**Live cdb evidence, directly ruling out two competing explanations.** Attached `cdb -pv` to
Xvfb's own Windows process (identified via matching `owner_pid`/winpid -- confirmed live this
pass that `LITEBOX_PROCESS_FORK=1` uses the real Windows PID as the guest PID directly, so the
two numberings coincide) and took two stack snapshots of its single guest-execution thread
several seconds apart. Both times the thread was inside `sys_epoll_pwait -> EpollFile::wait ->
Pollee::wait -> WaitContext::wait_on_events -> wait_until -> commit_wait ->
RawMutex::block_or_maybe_timeout` -- BUT the two snapshots' frame arguments differed (different
stack addresses on the `commit_wait` frame), proving this is a live, CYCLING bounded-repoll loop,
not a single permanently-parked wait -- ruling out "genuinely frozen thread" as the explanation
(the mistake the pass's own first read of a single cdb snapshot nearly made; two snapshots,
`docs/AGENTS_ARCHIVE_2026-09-18.md`'s own established technique from the 24th pass, is what
caught it). Source read of `litebox_shim_linux/src/syscalls/epoll.rs`'s
`has_unready_stdin_or_armed_timerfd_interest`/`repoll_stdin_and_timerfd_interests` (added
2026-09-18, BEFORE this session, per its own doc comment -- "AF_UNIX joins them as of
2026-09-18") shows the bounded-repoll-with-Unix-socket-awareness mechanism this exact scenario
needs ALREADY EXISTS and is architecturally sound on paper: any unready `EpollDescriptor::Unix`
interest unconditionally forces a bounded (`STDIN_REPOLL_INTERVAL`) re-poll instead of an
indefinite block, and the repoll calls `entry.poll(global)` on every registered stdin/timerfd/Unix
interest, pushing it into the ready set the moment `check_io_events_shared` (which reads the REAL
ring-buffer fill state fresh every call, confirmed correct by code inspection) reports it ready.

**Leading, NOT YET distinguished hypotheses for the next pass** (needs a THROTTLED
`syscalls::epoll=debug` -- e.g. mirroring `has_pending`'s 1-in-400 gate -- to get a usable trace of
what's actually in Xvfb's interest set without a repeat of the 78MB blowup): (a) Xvfb's own guest
code may never call `epoll_ctl(EPOLL_CTL_ADD, new_client_fd, ...)` on the just-`accept()`ed
connection at all -- if the connected socket is never added to the interest set,
`repoll_stdin_and_timerfd_interests` has nothing to check for it, matching the symptom exactly;
(b) a bug in `entry.poll(global)`'s own readiness bookkeeping for a freshly-`Shared`-transport
`Connected` `UnixSocket` specifically (as opposed to a `Local`-transport one, which is the
well-tested, pre-existing case) -- e.g. `is_ready` getting set/stuck incorrectly, or the entry's
`desc.upgrade()` failing for this specific descriptor shape; (c) something upstream of epoll
entirely -- worth confirming with a debugger breakpoint on `accept()`'s own return, in Xvfb's
guest code, that a real `epoll_ctl(ADD)` syscall follows it, before trusting (a)/(b) as the
narrower explanation. This is a DIFFERENT, deeper layer than anything the twelfth-through-
twenty-fifth-pass AF_UNIX rendezvous work (connection establishment, now confirmed genuinely
working end-to-end) ever reached -- the rendezvous protocol succeeds; the established
connection's actual byte-level conversation is what's silent.

**Host state.** Three more boot attempts this continuation (ten total across the whole
twenty-fifth pass), RAM held healthy throughout (2.6-5.8GB free, no proactive kill needed this
half), all process trees fully cleaned via WMI `Terminate` before each next launch and at session
end. Did NOT reach `[s] XVFB_UP`, `DBUS_UP`, `DE_LAUNCHED`, `SELKIES_PORT_UP`, or the browser/
terminal/apps milestone this pass. `.wfgy/xvfb_cdb_dump.txt`/`xvfb_cdb_snap2.txt` (transient,
gitignored, deleted after use) held the two raw cdb snapshots referenced above.

**Files touched this continuation**: `litebox_shim_linux/src/syscalls/unix.rs` (two new permanent
`debug!()` sites in `try_sendto_shared`/`try_recvfrom_shared`, kept).

## Twenty-sixth pass, 2026-09-20 — ROOT-CAUSED AND FIXED: Xvfb never reading `xset q`'s bytes; `[s] XVFB_UP` printed for the first time ever

Picked up exactly where the twenty-fifth pass's own pickup list left off: hypothesis (a) (does
`epoll_ctl(ADD, new_client_fd)` fire at all) and (b) (a readiness-bookkeeping gap specific to
`Shared`-transport `Connected` sockets) from that section above, tested with a real throttled
trace instead of further static reading.

**Instrumentation added** (`litebox_shim_linux/src/syscalls/epoll.rs`, `unix.rs`):
- `EpollFile::wait`'s old unthrottled per-~15ms-iteration `debug!()` ("DIAG EpollFile::wait: loop
  iteration") demoted to `trace!()` -- this alone was almost the entire 78MB/8min blowup the
  twenty-fifth pass flagged as unusable.
- `repoll_stdin_and_timerfd_interests`'s own per-cycle `debug!()` demoted to `trace!()` too; a new
  per-Unix-entry `debug!()` added in its place, 1-in-400-throttled on the routine "still not ready"
  case but UNCONDITIONAL the moment an entry is seen ready (or, post-fix, has a real event) --
  this is what actually caught the bug, see below.
- `add_interest`/`mod_interest` both gained an `is_unix` field on their existing per-call `debug!()`
  (both already low-volume -- 33 total `add_interest` calls across a whole boot -- so no new
  throttling needed), plus a NEW `debug!()` logging the raw `Events` returned by their own initial
  `file.poll(...)` call for any Unix descriptor.
- `SharedByteRing` gained `diag_cursor()` (returns `(write_pos, read_pos)`), logged from both
  `try_sendto_shared` (after the write) and a new throttled site in `check_io_events_shared`
  (1-in-100 + unconditional whenever `write_pos != 0`) -- built specifically to settle whether the
  peer's ring-cursor update is visible cross-process at all, independent of the higher-level
  `is_empty()` boolean.

**Live findings, in the order they closed off hypotheses**:

1. `epoll_ctl(ADD)` DOES fire for Xvfb's accepted client fd (confirmed fd=8 in one traced run,
   correlated via its `data=` pointer to the SAME fd across `add_interest`/`mod_interest`/repoll
   log lines). Hypothesis (a) REFUTED. But its own initial registration mask was EMPTY
   (`Events(0x0)`, or just the `EDGE_TRIGGER` bit alone) -- Xvfb's os/epoll layer reserves the slot
   first with no real interest, then immediately follows with a real `EPOLL_CTL_MOD` setting
   `EPOLLIN|EPOLLET`, ~70 microseconds later. This ADD-then-MOD pattern is a real, legitimate
   event-loop technique -- the investigation's OWN `add_interest`-only logging from the prior pass
   could never have seen the real mask this way, which is why it looked indistinguishable from
   "the real interest never gets registered."
2. The `SharedByteRing` cursor genuinely propagates cross-process: `try_sendto_shared`'s own
   `write_pos_after=12` (client side, `is_client=true`) is followed, within ~15-30ms (one to two
   bounded-repoll cycles), by `check_io_events_shared` on Xvfb's side (`is_client=false`) correctly
   reading `read_write_pos=12` and computing `Events(IN | OUT)`. The FIRST one or two checks
   immediately after `add_interest`/`mod_interest` (run synchronously, before that propagation
   window elapsed) legitimately still saw `write_pos=0` -- a real, small, and ultimately harmless
   race, NOT a shared-memory-visibility bug (that hypothesis, raised mid-pass, is REFUTED).
3. **The real bug, found by comparing the new per-Unix-entry repoll log against `mod_interest`'s
   own initial-poll log**: `mod_interest`'s own synchronous re-poll ran too early (still inside the
   ~15-30ms propagation window above) and saw `Events(0x0)` -- expected, not the bug. But the
   BOUNDED REPOLL, which runs every ~15ms specifically to catch exactly this kind of case, kept
   showing `event_mask=Some(1)` (a real, correctly-computed `EPOLLIN`) on EVERY cycle, for 60+
   consecutive cycles across multiple full boots, while its OWN `is_ready` field read `false` on
   every one of those same cycles. That is a direct contradiction inside `EpollFile::
   repoll_stdin_and_timerfd_interests`'s own logic, not a data-visibility question at all.
   `EpollEntry::poll` returns `(event: Option<EpollEvent>, is_still_ready: bool)`; the repoll
   function was branching on `is_still_ready` (the SECOND field) to decide ready-set membership.
   `is_still_ready` is DELIBERATELY forced `false` whenever the entry's own registration carries
   `EPOLLET`/`EPOLLONESHOT` (real edge-triggered semantics: "don't keep auto-reporting this
   forever", not "there's nothing to report") -- see `EpollEntry::poll`'s own body. Xvfb registers
   its accepted X11 client fd `EPOLLET` (confirmed above), so `is_still_ready` was unconditionally
   `false` for it regardless of real readiness, and the repoll's `if is_ready { self.ready.push(...)
   }` (using that field under the misleading local name `is_ready`) could NEVER push it, no matter
   how much unread data sat in the ring. `ReadySet::pop_multiple` -- the OTHER consumer of the same
   `(event, is_still_ready)` tuple -- already used the two fields correctly (`event` to decide
   whether to deliver, `is_still_ready` separately to decide whether to auto-requeue), which is why
   this exact bug shape never surfaced anywhere else: every other fd kind that reaches the bounded
   repoll (stdin, timerfd) is realistically always registered level-triggered, where the two fields
   happen to coincide, masking the distinction.

**The fix** (`repoll_stdin_and_timerfd_interests`): push to the ready set on `event.is_some()`
instead of `is_still_ready`, renaming the local binding to `has_event` for clarity. No other call
site needed a matching fix -- `pop_multiple` was already correct, and the initial `add_interest`/
`mod_interest` polls already used `!events.is_empty()` on the raw `Events`, an equivalent-safe
check that was never the bug (see finding 2's timing note for why those still sometimes read empty
regardless).

**Live verification, DEBUG binary, same `.wfgy/webtop_stack.sh` full boot**: after the fix,
`try_recvfrom_shared: read slot=0` fired repeatedly (the FIRST time this exact log line had ever
been observed in this entire multi-day investigation), followed by `[s] XVFB_UP` (never printed
before this pass, across dozens of boot attempts over 26 passes). The boot then reached `[s]
DE_LAUNCHED (image startwm.sh)` and attempted `[s] SELKIES_PORT_UP` before returning
`curl_exit=137` -- at that exact point host free RAM had fallen to ~320-480KB... (KB, not a typo:
roughly 320,000-480,000 KB free, i.e. ~0.3-0.5GB) with 19 concurrent cross-process-forked Windows
processes alive, and the run was deliberately WMI-`Terminate`d for host safety before determining
whether `SELKIES_PORT_UP`'s own failure is a real litebox bug or simply memory starvation at that
process count. Not yet re-attempted with more host headroom or on the RELEASE binary (which should
cost meaningfully less RSS per forked process than the DEBUG build, per the project's own standing
build-config notes).

**Not yet done this pass**: the release-binary + real-browser/terminal/apps milestone. Given how
far this pass got on the DEBUG binary (further than any of the prior 25 passes), this is the most
promising point this whole investigation has ever been at going into that attempt.

**Files touched**: `litebox_shim_linux/src/syscalls/epoll.rs` (the real fix, plus the trace/log
throttling and new diagnostic sites), `litebox_shim_linux/src/syscalls/unix.rs` (`SharedByteRing::
diag_cursor` + its two call sites). All diagnostic `debug!()` sites kept as permanent, low-volume
additions (matching this project's own established practice for high-value single-shot evidence
sites), consistent with everything already living in `try_sendto_shared`/`try_recvfrom_shared`
from the prior pass.

