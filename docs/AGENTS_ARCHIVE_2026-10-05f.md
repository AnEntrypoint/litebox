# AGENTS_ARCHIVE 2026-10-05f -- the dead listening port (Open #1), and the apps re-run (Open #2)

The verbatim `AGENTS.md` this pass started from is commit `5a0b6dc` (`git show 5a0b6dc:AGENTS.md`);
nothing else was carried forward from it except what the recompacted file kept. Everything below is
this pass's own evidence and mechanism.

## 1. Open #2 CLOSED: apps re-measured on `4964934`

`.wfgy/apps1d.sh` / `.wfgy/apps3d.sh` (the same scripts as `apps1`/`apps3`, new run names) on
`4964934`, xwd grab decoded mask-aware:

| app | census |
|---|---|
| `xterm` | red `255,0,0` 97.4% |
| `xclock` | blue 95.1% |
| `xcalc` | white 83.2% |
| `xeyes` | 155 distinct colours |
| `xedit` | white 94.2% |
| `chromium --app` | 800x600 uniform `240,192,32` 99.8% |
| `mousepad` (GTK3) | white 87.4% |
| `thunar` (GTK3) | white 60.9% |

**Zero `panicked at` in both `.err` files.** The `47cefb2` census is reproduced on `4964934`.

## 2. Open #1: why a published port stops answering, and what was fixed

Symptom (chrD91/chrD92, `.wfgy/chrD92.out`): `selkies=code=200` at t=30/60/90, then
`curl: (7) Failed to connect to 127.0.0.1 port 8081 after 60 ms` from t=120 to the end of the run --
while selkies keeps encoding (`EncFPS ~13`), the host browser keeps receiving frames, and an
in-guest `curl` to the UNPUBLISHED 9222 answers at the same instant.

### 2.1 The host side is NOT the wedge (pub1-pub5)

- `pub1` (guest `http.server` on a published port, host client): relay works end to end.
- `pub4` (`.wfgy/pub4.sh` + `.wfgy/pub4host.ps1`): guest served 8095 (published) / 8097 (published,
  no host client) / 8096 (unpublished control) and curled all four endpoints 45 times --
  **every in-guest curl returned 200 for all 45 iterations**, i.e. the guest side never died. The
  host probe's failures (`forcibly closed`, then 40x `actively refused`) are an ARTIFACT: the host
  probe outlived the guest run (host span 07:38:17 -> 07:44:58 = 401 s vs the guest's own 45x5 s =
  ~230 s), so it was connecting to a runner that had already exited. Do not re-read pub4 as a bug.
- `pub5` (`.wfgy/pub5.sh` + `.wfgy/pub5host.ps1`, guest does nothing but sleep, `-Publish
  8095:8095`): **29/29 host connects CONNECTED.** The host listener and its accept loop are healthy
  with no guest server at all.
- Gateway code read end to end (`litebox_platform_windows_userland/src/net.rs`): every inbound flow
  is reaped -- `pump_tcp_flows`'s `to_remove` removes the flow, releases `inbound_flow_ports` /
  `inbound_local_ports`, and `self.sockets.remove(handle)`. No leak found on the host side.
- The only way the host listener stops accepting is `spawn_publish_listener`'s
  `tx.send(...).is_err() -> return`, i.e. the gateway thread being gone; no panic of that thread
  appears in any run's `.err`.

### 2.2 The guest side: a listening port can lose every socket that listens on it

`litebox/src/net/mod.rs`:

- `TcpServerSpecific { backlog: Option<u16>, socket_set_handles: Vec<SocketHandle> }`. Because
  smoltcp's listening socket handles ONE connection at a time, `listen(backlog)` creates one
  listening socket per backlog slot (`refill_to_backlog`). **`backlog` is clamped to 8**
  (`backlog.min(8)` in `listen`).
- `accept` does `retain` (dropping stale/closed slots) then, ONLY on the success arm,
  `swap_remove(position)` + `refill_to_backlog`. The `NoConnectionsReady` arm returned without
  refilling.
- Stale slots happen: `socket_set_contains`'s own doc comment records a dead-holder
  `reset_after_poisoning()` wiping sockets out of the SHARED `SocketSet`, leaving a descriptor's
  handles dangling (live-caught as `"handle does not refer to a valid socket"`, killing a whole
  cross-process-fork child -- this was selkies' own `accept()`).
- Consequence, and this is the trap: once EVERY slot of a port is gone the port has no listening
  socket at all, so no later SYN can ever make a slot `Established`, so
  `drain_socket_channel_buffers`'s readable re-arm (which only fires when a slot IS `Established`)
  never fires either, so the server never calls `accept`, so nothing ever re-arms it. **The port is
  dead for the rest of the session while the connections it already accepted keep working** -- exactly
  chrD92 (selkies' accepted websocket streams on; new connects are refused in 20-60 ms).
- Second, independent way to lose a slot with no retry: `refill_to_backlog` stops early when
  `socket_set.iter().count() >= MAX_SOCKETS` (256) and warns
  `"listen backlog cannot be refilled: the socket table is full"`; nothing retried once the table
  drained. **That warn does NOT appear in chrD92's 8 MB `.err` (logscan: 0 hits) and neither does
  `backlog`**, so chrD92 was the stale-handle path, not exhaustion. 23736 `WARN` lines do appear
  there, so warn-level logging was live and the absence is real evidence.

### 2.3 The two fixes (both in `litebox/src/net/mod.rs`)

1. `accept`'s `NoConnectionsReady` arm: if the `retain` dropped any slot, log
   `diag-accept: listening backlog slot(s) went stale, re-arming the listener` (with `dropped=N`)
   and `refill_to_backlog` before returning the error.
2. `drain_all_socket_channel_buffers` now runs a second pass (`iter_mut_nowait`, separate from the
   drain's shared-guard pass so a guest thread blocked in `read()` cannot starve its own bytes) over
   every descriptor: `repair_listening_backlog` drops stale slots and re-arms ANY listening socket
   that is short of its backlog, whatever took the slot -- including a port whose every slot went
   stale (which never reaches `accept` at all) and a port a table-full refill left short. It logs
   `diag-listener: every backlog slot of this listening port went stale, re-arming it` (port=) only
   when the port went from armed to ZERO live slots, which is the visible symptom, so a persistently
   poisoned port cannot spam the log. It skips `consider_closed` descriptors (never re-arm a port
   being closed), never removes a socket that is still in the set (the sweep runs from every
   process's tick over a fork-family-shared socket set -- dropping another process's socket runs its
   ring buffers through the wrong heap, see `remove_dead_sockets`), and returns early when the socket
   table is full so the next tick retries instead of warning repeatedly.

Rule: **a listening port must re-arm itself from the tick, not only from `accept`.** Anything that
can take a backlog slot away (stale handle, a refill that failed under pressure, a future one) must
leave the port able to come back on its own.

## 3. Probe facts learned

- **`/proc/net/tcp` does not exist**: `/tmp/pub4net.py` got `FileNotFoundError(2)`. Don't build
  diagnostics on it.
- `LITEBOX_PUBLISH` parse/bind outcomes log at `info!` (`"published 127.0.0.1:H -> guest
  10.0.0.1:G"`), which the default `warn` filter hides. To see whether a port published, raise
  `LITEBOX_LOG`.
- `MAX_SOCKETS` is 256 and lives in a fixed shared arena; `listen` clamps backlog to 8.
- `SocketHandle` is `pub struct SocketHandle(usize)` with NO public index -- a live-handle bitmap is
  impossible; liveness is `socket_set.iter().any(|(h,_)| h == handle)` (`socket_set_contains`), and
  with backlog<=8 that scan is fine per tick.
