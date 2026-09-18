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
