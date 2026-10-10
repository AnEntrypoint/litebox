# Cross-process wake: the gaps that remain after the fd-carry fix (2026-10-10)

Three independent findings, all confirmed **by reading**, none yet proven causal for the
renderer exit. Two of them are latent parks with no escape hatch on Linux, which matters far
more now that the target is a whole desktop inside litebox (`docs/WEBTOP-IN-LITEBOX-2026-10-10.md`)
than it did for chromium alone.

## 1. `FutexManager` supports only PRIVATE futexes

`litebox/src/sync/futex.rs:28-29` — its own doc comment says it "only supports 'private'
futexes, since it assumes only a single process". `wait` ends in `cx.wait_until(...)` with
**no repoll** (`:129`).

Under `has_native_fork()` = unconditionally `true`
(`litebox_platform_linux_userland/src/lib.rs:3447`), a guest `clone()` becomes a real host
process, so any guest mutex initialized `PTHREAD_PROCESS_SHARED` (glibc then omits
`FUTEX_PRIVATE_FLAG`) waits on a futex word in `MAP_SHARED` memory and can only be woken by
another host process — which this table cannot do. That is a park with no escape.

Relevance: low for chromium alone (an A/B was set up and then dropped as not what kills the
renderer). **High for a desktop** — an X server, a compositor and their clients synchronize
through shared memory.

## 2. Every Linux host-identity oracle is a constant

(`litebox/src/platform/mod.rs`, audit `a618e`): `current_host_pid()` = `0` (`:1307`),
`is_process_alive()` = `true` (`:2018`), `cross_process_child_host_pid()` = `None` (`:1312`),
`wake_signal_listener` = `false` (`:1331`). `litebox_platform_linux_userland` overrides none.

Consequences: `xproc_send` is ESRCH, `poke_peer` is a no-op, and side/flock/record-lock/
listen-queue reclamation never fires. **But it does not cause spurious EOF** — `side_liveness`
(`unix.rs:4608`) reads `host == me == 0` and answers ALIVE, so `side_gone` never fires either.

## 3. There is no Linux tick

`net_worker` exists only with a tun (`runner/lib.rs:637`), `pump_tcp_flows` is Windows-only,
`fn tick` has 0 hits. Every cross-process wait degrades to a 15 ms re-poll
(`STDIN_REPOLL_INTERVAL`, `epoll.rs:1057`).

## The rank-1 stall

An unbounded `WaitState` park: `wait.rs:441` `condvar.block` → `linux lib.rs:2434`
`FUTEX_WAIT` with `timeout=None`. Dead-holder recovery is gated on `owner != 0`
(`linux lib.rs:2517`) while a condvar's `owner` is **always 0**, so recovery never runs and
the park is permanent the moment its wake lives in another host process.
`wait_on_events_polling` (`unix.rs:3939`) is the only existing mitigation.

## Status

None of the three is on the critical path for the renderer exit. All three are on the critical
path for a desktop. Revisit once `Xvfb` boots inside litebox — a client blocking on a reply
from the X server is exactly the shape that parks here.

Harness note: any experiment on this must sweep orphaned fork children first —
`docs/HARNESS-LESSONS.md`.
