# X11 over litebox's cross-process AF_UNIX ring — audit 2026-10-10

Read-only audit, ahead of booting a desktop inside litebox
(`docs/WEBTOP-IN-LITEBOX-2026-10-10.md`). Confirms the plan is viable on **message size** and
locates the real ceiling in **wake latency**.

## 1. A send larger than the ring: short write, no bytes lost

`SharedView::send` (`litebox_shim_linux/src/syscalls/unix.rs:1013`):

- `len <= SHARED_UNIX_CONN_BUF` → `try_write_all_with_fds` (`unix.rs:1100`), atomic, else `EAGAIN`.
- `len > 2048` → `try_write_with_fds(&msg.data[..2048])` (`unix.rs:1091-1098`) writing
  `n = min(len, free)` (`unix.rs:4282`), returning `Ok(n)`; `n == 0` → `EAGAIN`.
- `framed` slots → `EMSGSIZE` (`unix.rs:1076`), SOCK_SEQPACKET only — X11 is STREAM, never fires.

`UnixSocket::sendto` returns `Ok(n)` straight out (`unix.rs:2476`); only `EAGAIN` re-enters the
wait loop (`unix.rs:2477-2480`). So the caller is expected to continue — **no truncation, no
dropped remainder, and X11 clients/servers already loop on partial writes.**

Read side: `try_read_with_fds` returns `min(out.len(), readable_before_next_fd_mail(avail))`
(`unix.rs:4322-4330`), `avail <= 2048`, so a `recv` never returns more than 2048. Byte stream,
no reassembly needed. `readable_before_next_fd_mail` (`unix.rs:4178`) can shorten a read to
avoid swallowing a later fd-bearing message; harmless for a stream.

## 2. 2048 is a per-send cap on a refillable ring — latency, not size, is the cost

Ring = fixed 2048 B with `write_pos`/`read_pos` cursors (`unix.rs:4104-4107`, `4280-4297`). A
sender can push 100 KiB as ~50 chunks as the receiver drains.

**But there is no cross-process wake on Linux.** `poke_peer` (`unix.rs:955`) calls
`platform.wake_signal_listener`, `false` at `litebox/src/platform/mod.rs:1331`. A waiter parks
and is re-polled at `SHARED_UNIX_POLL_INTERVAL = 15 ms` (`unix.rs:3931`,
`wait_on_events_polling` `unix.rs:4003`).

→ ~2048 B per 15 ms ≈ **136 KB/s per direction**, and **~15-30 ms per request/reply round trip**.
X11 is round-trip heavy at startup (font queries, property gets). A 100 KiB `X_QueryFont` reply
≈ 0.75 s; a 1 MiB `X_PutImage` ≈ 8 s. **Under `timeout` this is indistinguishable from a hang.**

Levers: a real shared-futex wake (arena is `MAP_SHARED`, so a host futex word crosses host
processes), or raising `SHARED_UNIX_CONN_BUF` at 4096 slots × 2 directions × B (2048 → 16 MB
today; 64 KiB → 512 MB, likely unaffordable).

## 3. fd passing: Xvfb needs none, DRI3 would kill the message

`scm_carry_spec` (`file.rs:10621-10693`) covers `F|` file/dir, `E|` eventfd, `S|` shm/memfd,
`U|` unix, `N|` inet, else `T|` snapshot, else `Err`. An uncarryable fd makes the **entire**
`sendmsg` return `EOPNOTSUPP` (`unix.rs:1034-1049`). Reachable subsystems have **no drm**
(`file.rs:11286-11303`).

MIT-SHM is SysV `shmget`, not an fd, so **with Xvfb (no DRI3) X11 passes zero fds** — moot.
If chromium asks for DRI3 (`X_DRI3Open`) or a syncfile fd, that fd is uncarryable →
`EOPNOTSUPP` → the whole message dies. Also `E|` rebuilds as a **process-local** eventfd
(`file.rs:10739-10748`), so even when carried it cannot wake the peer.

## 4. memfd naming: aliasing confirmed, and the name IS load-bearing

- `file.rs:3070-3074` name = `Local\litebox_memfd_{current_host_pid()}_{MEMFD_OBJECT_COUNTER}`.
  `current_host_pid()` is 0 on Linux (`litebox/src/platform/mod.rs:1307`); the counter is a
  process-local `static AtomicU64` (`file.rs:5172`) that a real `fork()` **copies**, so parent
  and child both next issue the same N.
- `create_named_shared_memory` FNV-1a-hashes the name (`platform/lib.rs:3081-3084`) →
  `pool_segment(key|1, size)` (`platform/lib.rs:3092`), which **returns an existing entry by key
  with no size check and no exclusivity** (`shared_heap.rs:107-113`).

Two logically distinct memfds in a parent and its fork child therefore **alias the same segment
with no errno**, and the aliased segment is returned regardless of requested size, so a size
mismatch maps past it into the next bump-allocated region. `file.rs:3534-3558` already guards
this exact mechanism for the *carrier* name with `O_EXCL` + 64 retries; the **object** name at
`:3070` has no guard.

**The reverse risk is real:** the carry path relies on the name resolving to the sender's segment
(`S|{flags}|{size}|{name}` `file.rs:10646` → `install_shm_file` → `create_named_shared_memory`
`file.rs:3525-3529`). **Adding a process id would break it** and would be a no-op on Linux anyway
(pid is always 0). Correct fix = *object* uniqueness (move the counter into the shared arena) or
make the token path primary — `shared_memory_token_for` (`platform/lib.rs:3095`) yields
`lbxpool:{handle:x}`, already resolved exactly and collision-free at `platform/lib.rs:3073-3079`.

## 5. The unbounded park is real, but X11's data path already avoids it

`commit_wait` uses `condvar.block` with no deadline (`wait.rs:441`); `RawMutex::block` passes
`timeout=None` unless `owner != 0`, always 0 for a condvar (`platform/lib.rs:2515-2519`) →
unbounded `FUTEX_WAIT` (`platform/lib.rs:2445`).

But unix `send`/`recv` go through `wait_on_events_polling` (bounded 15 ms, `unix.rs:2456`,
`2506`), and `epoll_wait` (`epoll.rs:389`, `EpollDescriptor::Unix(_) => true` at `epoll.rs:458`)
and `PollSet::wait` (`epoll.rs:1219-1226`, repoll `:1258`) already include AF_UNIX in the
bounded-repoll set — the comment at `epoll.rs:432` names Xvfb and dbus-daemon. **So the
mitigation is real and already applied.**

Residual, and not X11-specific: a guest pthread condvar/futex wait with **no timeout** whose
wake must come from another host process (xfwm4/GTK on a condition variable, a shm-based
compositor handshake). Those park forever. See `docs/CROSS-PROCESS-WAKE-2026-10-10.md`.

## Bottom line

Not blocked by message size. Blocked by ~136 KB/s and 15-30 ms per hop, plus `EOPNOTSUPP` if
the guest ever passes a drm/sync fd. Item 4 is a live silent-memory-aliasing bug a desktop will
hit harder than chromium does — and its fix must preserve name-based sharing.
