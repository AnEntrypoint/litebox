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
