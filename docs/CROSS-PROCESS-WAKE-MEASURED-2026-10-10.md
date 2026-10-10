# Cross-process AF_UNIX wake: implemented and measured (2026-10-10)

Commit `10fa53cd`, `litebox_shim_linux/src/syscalls/unix.rs` only. Switch
`LITEBOX_UNIX_CROSS_WAKE_OFF` (HOST env; ON = unset, OFF = `=1`).

## What changed

- `SharedWakeWord<Platform>` (`unix.rs:4012`) — one `Platform::RawMutex` word plus a waiter count.
  `bump()` (`4022`) increments the word and, only if `waiters != 0`, calls `wake_all()`;
  `wait_changed(seen, timeout)` (`4036`) registers as a waiter and does a **bounded**
  `block_or_timeout(seen, timeout)` — never an unbounded park.
- `SharedConnSlot.wake` (`unix.rs:4689`), initialized in `new_empty()`; `SharedUnixConnTable::wake_word(slot)`
  (`~4716`); `free()` bumps the word so a slot teardown cannot leave a waiter asleep.
- `poke_peer` (`unix.rs:~4150`) bumps it before the existing `wake_signal_listener` loop, which is
  dead on Linux (`wake_signal_listener` = `false`, `current_host_pid()` = `0`).
- `SharedWake` trait + `SharedWakeHandle { global, slot }` (`unix.rs:4057`) — **owned**, so the
  connection state lock is not held across the park.
- `UnixConnectedStream::shared_wake()` (`unix.rs:1432`) → `None` for an unpromoted `Local` pair and
  for the OFF switch.
- `wait_on_events_polling_woken` (`unix.rs:4140`): `wait_on_events_polling` is now a thin wrapper.
  With a wake word, the inner `wait_on_events` park is narrowed to `Duration::ZERO` (it still runs
  `try_op` and still collects in-process events) and the full tick is spent in `wait_changed`, so a
  bump can never be lost — the word is sampled *before* `try_op`. Boundedness, `has_real_deadline`
  and `remaining` discrimination are unchanged.
- `UnixStream::sendto` (`:2492`) and `recvfrom` (`:2548`) pass the word in.
- `litebox_runner_linux_userland/src/lib.rs:840`: tar mode mask `& 0o777` → `& 0o7777` (the Windows
  runner already used `0o7777`; `Mode::SVTX` exists, so only this platform dropped sticky/setuid).

## The measurement — and the shape it needs

**Three shapes do NOT reach the shared ring; only a carried descriptor does.**

| shape | reaches `SharedByteRing`? | evidence |
|---|---|---|
| `socketpair()` carried across `fork` | no | 0 `diag-xwake` |
| `bind`/`listen` in parent, `connect` in child | no | 0 `diag-xwake` |
| `bind`/`listen` in a **sibling** of the client | no | 0 `diag-xwake`; `TRACE unix_connect: entry` → `result ok=true` with no `connect_cross_process` branch (log-s1dbg.txt) |
| `socketpair()` end passed to a sibling over **SCM_RIGHTS** | **yes** | `diag-xwake` fires |

Reason: the guest AF_UNIX address table lives in the shared arena, so `lookup` finds a sibling's
backlog and `try_connect` builds an in-process (`Local`) endpoint whose channels are already
shared-arena resident and already wake fine. Only a descriptor that must be *rebuilt* in the
receiver (`file.rs:10636`) becomes `Shared`, which is exactly the chromium/Mojo shape.

**Second finding, now closed: with a socket timeout the 15 ms is in the POLL path.** CPython waits
for readiness before `recv` whenever `settimeout` is set, and neither `PollSet::wait`'s bounded
repoll (`epoll.rs`) nor `EpollFile::wait`'s consulted the wake word. `probe73` with `settimeout(60)`
measured **15.927 ms mean even with the fix ON**, and every real consumer (Xvfb, dbus-daemon,
chromium's Mojo) waits for readiness before reading. See the next section.

## BOTH WAYS (`probe73.py`, blocking sockets)

Two sibling processes, one `socketpair()` end carried over SCM_RIGHTS, 40 × 1-byte round trips, then
64 KiB in 4096-byte writes.

**Release binary**, md5 `1221a7b51002407446c3cd1900229c44` (logs `log-ron.txt` / `log-roff.txt`):

| arm | RTT mean | RTT median | RTT min | RTT max | throughput | `diag-xwake` |
|---|---|---|---|---|---|---|
| ON (switch unset) | **0.273 ms** | **0.069 ms** | 0.038 | 4.474 | **21.9 MB/s** | 4 lines, incl. `parked ... timeout_ms=15` |
| OFF (`=1`) | 17.201 ms | 15.990 ms | 15.117 | 32.904 | 0.128 MB/s | 1 line (bump from `free()`), **no `parked`** |

**232× lower median RTT, 172× more throughput.**

**Debug binary**, md5 `5fb70a68eef8989ae77b582e8aa2b66f` (logs `log-c2on.txt` / `log-c2off.txt`) —
same direction, smaller gap because a debug shim's own syscall cost is a bigger share of the wake
path: ON 0.247/0.216 ms and 4.93 MB/s vs OFF 16.101/15.315 ms and 0.134 MB/s (65× / 37×).

The switch discriminates on both: `parked` appears only in ON.
Harness `/tmp/lbx/xwrun.sh`, probes `/tmp/lbx/probe7{0,1,2,3}.py`.

## The poll path (`epoll.rs`), commit `6f58c5ff`

Same switch. `unix.rs` gains `UnixStream::shared_wake()`, `UnixSocket::shared_wake()` and
`unix_fd_shared_wake(global, &TypedFd)` — the last so a path that sees only a descriptor (poll,
`ppoll`, `select`, `epoll_wait`) can reach its connection's word. `epoll.rs` gains
`park_on_unix_wake_words(words, seen, budget)`: words cannot be waited on together (no multi-futex
wait here), so each gets a share of the budget and the whole set is re-checked between shares — a
bump on ANY word is noticed within one share. **Bounded by construction**: the budget is always a
real finite duration and there is deliberately no unbounded variant.

`PollSet::wait` and `EpollFile::wait` then snapshot every word **before** scanning (a bump racing
the scan reads as already changed, so no wake is lost), shrink their own condvar park from 15 ms to
`POLL_WORD_CONDVAR_SLICE` = 200 µs — that park can only ever be interrupted in-process, and an
in-process event still interrupts it immediately — and park on the words for the remainder. The word
park is capped at `POLL_WORD_PARK_SLICE` = 2 ms, **not** at the full interval: a raw futex wait
cannot be interrupted by this process's own `Waker`, so capping it keeps an event on an in-process
member of the same set at ~2 ms instead of making it wait out a whole interval. An empty word set —
a datagram, a same-process (`Local`) socket, or the switch OFF — is exactly today's path.

### BOTH WAYS (`probe74.py` = `probe73.py` + `settimeout`, the poll-then-read shape)

Release binary, md5 `2220690e50d3ea9042a32bd158389d06` (logs `log-p5on.txt` / `log-p5off.txt`),
40 × 1-byte round trips then 64 KiB in 4096-byte writes, both arms rc=0, `hostprocs=0` after:

| arm | RTT median | RTT mean | RTT min | throughput | log bytes | `diag-xwake` |
|---|---|---|---|---|---|---|
| ON (switch unset) | **0.308 ms** | 0.343 ms | 0.306 | **6.88 MB/s** | 1,962 | `parked ... timeout_ms=2` |
| OFF (`=1`) | 15.173 ms | 17.447 ms | 15.151 | 0.142 MB/s | 2,807 | 1 line (bump from `free()`), **no `parked`** |

**49× lower median RTT, 48× more throughput.** Before this commit the same probe measured 15.927 ms
with the fix ON; the untuned first cut of it (500 µs condvar slice, 15 ms word park) measured
0.732 ms / 2.96 MB/s at md5 `b837ad0e2796dcc0d446bbbad223281d`, so the tuning is worth ~2.4× and
the cap is what buys it. Harness `/tmp/lbx/xwrun.sh`, probes `/tmp/lbx/probe7{3,4}.py`.

## Real-app arm: the webtop's own `xwd -root` capture is NOT ring-bound

Same binary (md5 `2220690e50d3ea9042a32bd158389d06`), rung 4 of the webtop boot (`rootfs-webtop7.tar`),
timing every `xwd -root` capture from inside the guest (`/tmp/lbx/rung4wrap.py` wraps `boot.py`'s
`subprocess.run`; harness `/tmp/lbx/xwrung4.sh`). A capture is **3,148,907 B**, so if it crossed a
2048-byte ring at 15 ms per repoll it would take tens of seconds.

| arm | captures | per-capture elapsed | median | `parked` | `bumped` | rc |
|---|---|---|---|---|---|---|
| ON (switch unset) | 7 | 0.174, 0.160, 0.233, 0.202, 0.202, 0.210, 0.337 | **0.202 s** | **168** | 6 | 0 |
| OFF (`=1`) | 7 | 0.373, 0.208, 0.231, 0.210, 0.198, 0.210, 0.203 | **0.210 s** | 0 | 7 | 0 |

**Identical.** Both arms also ran the whole desktop to a clean exit (`rc=0`, ~22 MB of guest stdout
each). The poll path really did park on the word in the ON arm (168 parks) — it just was not on this
capture's critical path, which is the whole point: **Xvfb↔`xwd` is a `Local` connection.** `sys_connect`
resolves the shared-arena address table and builds an in-process endpoint whose channels are already
arena-resident, so client↔server X11 traffic never touches `SharedByteRing` at all.

So a slow `xwd` capture on this track is **not** the ring, and `SHARED_UNIX_CONN_BUF` is not its
lever — look at host contention (45 guest processes and a spinning chromium on 4 CPUs) instead. The
wake still matters where the ring IS used: chromium's Mojo, where the fd is rebuilt in the receiver.

## Arena arithmetic (`SHARED_UNIX_CONN_BUF`)

`SHARED_UNIX_CONN_CAPACITY` = 4096 slots, 2 rings/slot, arena region is power-of-two sized.

| `SHARED_UNIX_CONN_BUF` | ring bytes alone | + per-slot overhead (~5.4 KiB/slot: 2 × `RingCursor` with `fd_mail` 2 × 664 B, holder rows, creds) | region |
|---|---|---|---|
| 2048 (today) | 16 MiB | ~37 MiB | 64 MiB |
| 4096 | 32 MiB | ~53 MiB | 64 MiB |
| 8192 | 64 MiB | ~85 MiB | 128 MiB |
| 65536 | 512 MiB | ~0.6 GiB | unaffordable (Windows arena is 128 MiB) |

**The wake word costs 12 B/slot → 49 KiB for all 4096 slots** (one word per slot, not per ring:
the two waiters are this connection's two ends). Raising `SHARED_UNIX_CONN_BUF` to 4096 is the only
cheap doubling and it does **not** address latency — it halves the number of wakeups, while the wake
itself removes the 15 ms from each one. Do the wake first.

## Repro

    cd /tmp/lbx && PROBE=probe73.py SECS=180 bash /tmp/lbx/xwrun.sh c2on          # blocking recv
    cd /tmp/lbx && PROBE=probe74.py SECS=200 bash /tmp/lbx/xwrun.sh p5on          # poll-then-read
    cd /tmp/lbx && PROBE=probe74.py SECS=200 bash /tmp/lbx/xwrun.sh p5off LITEBOX_UNIX_CROSS_WAKE_OFF=1
    cd /tmp/lbx && PROBE=probe73.py SECS=180 bash /tmp/lbx/xwrun.sh c2off LITEBOX_UNIX_CROSS_WAKE_OFF=1
