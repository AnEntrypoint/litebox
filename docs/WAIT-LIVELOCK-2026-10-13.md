# Guest livelock in `wait_until` (2026-10-13)

Status: **open**. The webtop/chromium run stalls in about 1 run in 10 (6 stalls in about 61 runs
on the diagnostic binaries). No fix is claimed. Reliability is not "always" yet.

## What a stall looks like (measured)

- Runner exits 124 at the 420 s cap, or exits 0 early (after about 257 s, 5 frames). Either way
  stdout is 167 bytes (`hr4`, `hs2`, `hi2`, `hk1`, `hn12`) or about 15.7 MB with 5 frames
  (`hn10`, `hp2`), and no frames are captured after the stall begins.
- `log` shows two threads alternating `[diag-longwait] enter timeout_ms=N` with
  `exit elapsed_ms=0 ok=true`. N is constant for a pair (for example 60240 and 29036 in `hn12`),
  so each park is a re-entry of **one** `wait_until` whose remaining time does not advance
  across a pair. The loop is in `wait_until`, not in fresh calls.
- The loop body is `litebox/src/sync/futex.rs:129`
  (`cx.wait_until(|| entry.get().done.load(..))`), the guest FUTEX_WAIT. The flagged spin reports
  name it, with `epoll.rs:1367` as the other.
- Host threads are NOT spinning: `thr-hp2.txt` (sampler, 20 s period) shows runner threads in `S`
  (`futex_do_wait`, `hrtimer_nanosleep`, `do_wait`) with no `R` samples. `load` is about 1.1 in a
  stall. Each park returns `Ok` in 0 ms, so each iteration costs a wake, not CPU.
- `wait_sample` is 0 or near 0 in a stall (healthy runs: 40 to 60 samples, about 5M
  `wait_until` entries). Entries are counted at entry, so a stall shows few entries and many
  re-parks.

## Ruled out by reading the code

- **Wake targets the wrong futex.** `litebox/src/sync/futex.rs` `wake` (line 152) matches on
  `entry.addr == addr` and on `bitset` before waking. A wake reaches only its own entry.
- **EAGAIN spin.** `ImmediatelyWokenUp` logs `ok=false`; the stalled parks log `ok=true`.
- **Heap-lock deadlock.** The earlier `shared heap lock stuck` (tw7) did not recur: 0 of
  about 60 runs on the diagnostic binaries.
- **A 30 s loop check catches it.** `diag-wait-spin` fires at 1024 re-parks after 30 s, which
  healthy waits also reach (about 108 s into a desktop run, 2 to 4 reports per run). It does not
  separate stall from healthy. The end line `diag-wait-spin-end` is the discriminator: a healthy
  flagged wait returns, a stalled one does not (`spin_end=0` in hp2 and hn12).

## Open hypotheses (untested)

1. **Stale or shared thread waker.** `epoll.rs` (`register.then_some(cx.waker())`, lines 1367
   and 1396) registers the thread's waker on sockets. A later socket event wakes that waker while
   the thread sits in an unrelated futex wait. Each wake returns `Unblocked`, `done` stays
   false, and the loop re-parks. Test: count wakes on the futex waiter's waker that do not set
   its `done`, and log the registering source.
2. **Wake storm from a peer.** The other thread in the pair wakes this thread on an address this
   thread does not wait on, through a shared waker. Test: log the guest `uaddr` in the FUTEX_WAIT
   entry and at each flagged end.

## Instruments in the tree (all env-free except where noted)

- `litebox/src/event/wait.rs`: `wait_until` is `#[track_caller]`; `diag-wait-sample` every 100k
  entries (names the dominant caller); `diag-wait-spin` at 1024 re-parks after 30 s;
  `diag-wait-spin-end` when a flagged wait returns; `diag-wait-interrupted` at 200k
  interrupts. All use `litebox_util_log::warn!`, which prints only when `LITEBOX_LOG` admits warn.
  **Run with `LITEBOX_LOG=warn`** or they are silent (this was the earlier mistake: 0 reports
  looked like proof).
- `litebox_platform_linux_userland/src/shared_heap.rs`: `LITEBOX_DIAG_HEAPLOCK=1` dumps the
  lock owner's `/proc/<tid>/{stat,wchan,syscall}` at 20M spins.
- `litebox_platform_linux_userland/src/lib.rs`: `LITEBOX_DIAG_LONGWAIT=1` logs timed parks of
  10 s or more.

## Batches (all on the same guest; `run-wt.sh`, 420 s, `--disable-gpu --in-process-gpu`)

| batch | binary | runs | result |
|---|---|---|---|
| hl1-hl6, hr1-hr3, hr5-hr8, hs1, hs3 | heap-lock diagnostic | 14 | clean |
| hr4 | heap-lock diagnostic | 1 | stall, 420 s |
| hs2 | heap-lock diagnostic | 1 | stall, 420 s |
| hi1, hi3-hi8 | interrupt counter | 7 | clean |
| hi2 | interrupt counter | 1 | stall, 420 s |
| hj1-hj8 | time-based 30 s check | 8 | clean |
| hk1 | time-based 30 s check | 1 | stall, 420 s |
| hk2 | time-based 30 s check | 1 | clean |
| hm1-hm8 | sampling (`LITEBOX_LOG=warn`) | 8 | clean |
| hn1-hn9, hn11, hn13-hn16 | sampling | 13 | clean (hn14 noisy: load 19) |
| hn10 | sampling | 1 | early exit, 5 frames |
| hn12 | sampling | 1 | stall, 420 s |
| hp1, hp3-hp8 | sampler + end line | 7 | clean (one flagged wait returned in each) |
| hp2 | sampler + end line | 1 | early exit, 5 frames, flagged waits never returned |

`wait_sample` and `diag-wait-spin` counts are per-run; the line counts above come from
`awk` over `log-<tag>.txt` in the scratchpad (not committed).

## Commits

`b9f8d0b2` (interrupt counter), `059c44a3` (re-park counter), `2ed0973e` (30 s check),
`189fc4ed` (wait-entry sampling), `9f0a62bc` (flagged-wait end line), `0063ccc4`
(heap-lock owner dump). Author lanmower.

## Next step

Test hypothesis 1 first: count `done == false` wakes on the futex waiter's thread waker, and log
the registering path from `epoll.rs`. Run one stall-catching batch with the counter; a stall
should show the count climbing while `spin_end` stays at 0.
