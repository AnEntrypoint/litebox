# Carrying an AF_UNIX SOCK_DGRAM socket across a cross-process fork (2026-10-08)

Open item 2: `UnixSocket::fork_carry_check` refuses a bound-or-connected SOCK_DGRAM socket
(`unix.rs:5149`, `Err("unix-datagram(bound-or-connected)")`), so `try_cross_process_fork` falls back
to the in-process RELOCATING fork, where the child dies of a SIGSEGV. A `SOCK_DGRAM` socketpair
counts as bound-or-connected, so this hits ordinary `socketpair()` users. Chromium does not hit it
(chrF42: `not_eligible` refusals 0).

Two ways to fix it. The second is blocked; the first is designed but NOT implemented.

## Why the two candidate fixes, and which is which

A refusal must degrade to a WORKING fork. The relocating fallback is the semantically exact answer
-- an in-process fork genuinely SHARES the socket object, as Linux does -- so fixing it fixes every
refusal shape at once (dgram bound/connected AND `unix-socket(connect-in-progress)`, `unix.rs:5139`).
The carry is an approximation, but it is the well-trodden path (hundreds of chromium forks/hour).

## Branch B: fix the relocating fork's unhealable store -- BLOCKED

`fork_verify.rs:1598-1626`: a write whose target `address` is in the SOURCE range first tries
`translate_memory_operand_registers` (`:1803`, rewrites `memory_base`/`memory_index` and retries the
same instruction). When that fails it warns "stale DATA pointer write detected" (`:1620`) and returns
`StepOutcome::StalePointer{is_write:true}` (`:1622`) -> synthetic `EXCEPTION_ACCESS_VIOLATION`
(`access_violation_record` `:2343`) -> guest SIGSEGV.

Healing it means emulating the store at `relocations.translate(address)` (same bytes, same length) and
advancing rip. **There is no XMM access anywhere in `fork_verify.rs`**: `register_value` (`:2308`) is
GPR-only. A 16-byte `Movups` store keeps its value in an XMM register, so this needs new CONTEXT
(XMM/FltSave/XState) plumbing first -- and a wrong value written through `relocations.translate` risks
the parent's memory, which is exactly what this file exists to protect. Do not attempt without that
plumbing and a clear picture of which CONTEXT flags the VEH handler requests.

## Branch A: carry the datagram -- designed, not implemented

### What a bound/connected dgram holds (`UnixDatagramInner`, `unix.rs:2580-2597`)

- `addr: Option<BoundDatagramAddr>` -- the bound name (type at `:2575`), the global it registered in,
  and the pid owning the `unix_addr_presence` row.
- `recv_channel: Option<ReadEnd<Platform, DatagramMessage>>` -- inbound queue; created by `bind`
  (`:2637`) and `new_pair` (`:2696`).
- `connected_send_channel: Option<(WriteEnd<Platform, DatagramMessage>, UnixSocketAddr)>` -- default
  peer; set by `connect` (`:2771`) and `new_pair` (`:2703`/`:2714`).
- `DatagramMessage { data, source, cred }` (`:2486`) -- bytes plus sender address and creds, no fds.

The channels are in-process `crate::channel::{ReadEnd, WriteEnd}`, NOT arena-shared. `fork_carry_check`
keys on those three fields being `None`, not on `addr` -- so an UNNAMED socketpair, which `new_pair`
(`:2690`) cross-wires with both a recv and a connected-send channel, is refused as "bound-or-connected".

### The machinery to reuse (stream already has all of it)

`fork_carry` (`:5172`) emits spec kinds `I,{seq}` / `B,...` / `L,...` / `C,{seq},{slot},{is_client},{me},{peer},{local},{peer}`
(`:5238-5250`). The `C` arm calls `conn.promote_for_fork` (`:1355`), which allocates a slot, stores
`framed = preserve_boundaries`, `hold()`s its own side (and the peer's, unless peer-shutdown), and
drains queued messages into the ring; `fork_carry` then adds one extra `hold(is_client, me)` for the
child (`:5259`) and returns `UnixCarryHold::Conn{slot,is_client,host}`. The child rebuilds in
`from_fork_spec` (`:5417`). Lifecycle: `fork_carry_move_to_child` (`:5280`), `fork_carry_abandon`
(`:5312`), `release_unadopted_carry` (`:5352`).

`SharedUnixConnTable` (`:4455`, 4096 slots at `:3823`, `arena_backed` disables it on exhaustion,
`alloc` reclaims dead holders) holds `SharedConnSlot` (`:4194`): two `SharedByteRing<_, 2048>`
(`:3841`), both-side creds, `holder_hosts/holder_counts: [[AtomicU32;32];2]`, `framed: AtomicBool`
(`:4212`). Pointer-free by design.

### `framed` is already datagram-shaped

Set only from `preserve_boundaries`, which is `matches!(ty, SockType::SeqPacket)` (`:3195`) -- i.e.
today only promoted SOCK_SEQPACKET pairs use it. Write is `try_write_record_with_fds` (`:4113`) =
4-byte LE length + data, all-or-nothing, `EMSGSIZE` over the ring; read is `try_read_record_with_fds`
(`:4127`), consuming exactly one record and truncating the tail. **Message-boundary-preserving**, which
is exactly datagram semantics. Only loss: `DatagramMessage.source` and `.cred` (bytes-only ring; one
cred per direction via `last_writer_*`).

### The change

1. `fork_carry_check` (`:5140-5151`): drop the blanket dgram refusal.
2. New `UnixDatagram::promote_for_fork`, modelled on `:1355`: alloc slot, `framed = true`,
   `hold(is_client, me)` twice (self + child); for a socketpair also `hold(!is_client, me)`; for a
   bound socket additionally register presence as the `L` arm does (`:5205`).
3. `fork_carry`'s dgram arm (`:5183`) -- today `"{status:x};D"`, which `from_fork_spec:5423` rebuilds
   as a FRESH EMPTY socket, i.e. silently losing the connection -- becomes
   `"G,{slot},{is_client},{me},{local_addr},{peer_addr}"`.
4. `from_fork_spec`: new `G` branch rebuilding a `UnixDatagram` with a slot-backed transport (new
   transport variant; `recv_channel`/`connected_send_channel` = `None`), re-binding the name via
   `addr.bind(task, false)` as the `B` branch does (`:5434`).
5. `release_unadopted_carry` (`:5352`) parses only `C` -- must learn `G`.
6. Update the SCM_RIGHTS warning text (`:1008-1013`).
7. Send/recv must prefer the ring whenever a slot exists (as stream does at `:1487`/`:1704`).

Caveat, stated honestly: bound-name delivery to a THIRD process still needs a shared
`unix_addr_table`. That table is per-process (`lib.rs:3685`; dgram send end inserted at `:2640`,
`UnixEntryInner::Datagram` `:3429`), so a cross-process sender already misses it
(`lookup` `:2732` -> `log_cross_process_presence_miss` `:3625` -> ECONNREFUSED) -- a pre-existing gap
independent of fork. Parent<->child over a carried socketpair works because both ends attach to the
same slot; a carried NAMED dgram stays locally deliverable only, which is still strictly better than
the current SIGSEGV.

## How to prove it

`.wfgy/unixrx1.sh` is the existing probe: both dgram arms read `status=11` (SIGSEGV) today. A fix must
turn both into a working fork (`CHILD_...=OK`), with the INVALID guard still `rc=1 bytes=0`, and with
`unix.rs`'s stream arms unchanged. Both ways: `LITEBOX_..._OFF`-style switch if the new carry is
gated at all, or simply the before/after of the same probe.
