// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Network-related functionality

use alloc::vec;
use alloc::vec::Vec;
use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::event::Events;
use crate::net::socket_channel::NetworkProxy;
use crate::platform::{Instant, TimeProvider};
use crate::sync::RawSyncPrimitivesProvider;
use crate::{LiteBox, platform, sync};

use bitflags::bitflags;
use smoltcp::socket::{raw, tcp, udp};

pub mod errors;
pub mod local_ports;
mod phy;
pub mod socket_channel;

#[cfg(test)]
mod tests;

use errors::{
    AcceptError, BindError, CloseError, ConnectError, ListenError, LocalAddrError, ReceiveError,
    RemoteAddrError, SendError, ShutdownError, SocketError,
};
use local_ports::{LocalPort, LocalPortAllocator};

/// IP address for LiteBox interface
// TODO: Make this configurable
pub const INTERFACE_IP_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);

/// IP address for the gateway
// TODO: Make this configurable
pub const GATEWAY_IP_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);

/// Size of each socket rx/tx buffer. Buffers come from fixed slot pools in the shared kernel
/// arena (see `socket_buffers`), so this is a pool slot size, not a per-socket allocation.
pub const SOCKET_BUFFER_SIZE: usize = 65536;

/// Size of one smoltcp socket's rx (or tx) ring, i.e. one pool slot.
///
/// A TCP socket costs two slots, so `MAX_DATA_SLOTS / 2` is how many sockets the pool can hold and
/// `MAX_DATA_SLOTS * SOCKET_RING_SIZE` is the whole pool. Both numbers are set together against
/// the shared arena's budget -- the arena also carries every `GlobalState` of the session, and
/// [`socket_buffers::Pool::new`] silently halves a request it cannot grant, so a too-large pool
/// silently becomes a too-small one. 32 KiB rings buy twice the sockets for the same 16 MiB, and
/// a desktop session runs out of SOCKETS long before it runs out of per-socket window: chrD97
/// measured 128 sockets all in use, 104 of them merely armed backlog slots of 13 listening ports,
/// after which every listening port whose next refill failed went deaf for the rest of the run
/// while the connections it had already accepted kept streaming.
pub const SOCKET_RING_SIZE: usize = 32768;

/// Limits maximum number of packets in a buffer
const MAX_PACKET_COUNT: usize = 32;

/// Fixed capacity of [`Network::socket_set`]'s slot table.
///
/// `Network` is embedded by value inside `litebox_shim_linux::GlobalState`, which on a platform
/// with genuine cross-process shared kernel state (`SharedKernelStateProvider`) is placed once
/// into a shared arena and then ATTACHED to (never reconstructed) by every other process in a
/// cross-process-fork family (see that trait's own doc comment). `socket_set` itself, however,
/// used to be `smoltcp::iface::SocketSet::new(vec![])` -- an ordinary growable `Vec`, whose
/// HEADER (ptr/len/cap) lives inline in that shared struct (so it copies over fine) but whose
/// BACKING BYTES are an ordinary heap allocation on whichever process's PRIVATE heap first grew
/// it. Every process other than the one that ran `Network::new` therefore held a `Vec` pointer
/// meaningless (or dangling) in its own address space -- the confirmed root cause of a live
/// `tcp::Socket::dispatch` panic (`self.tuple.unwrap()` on `None`, `smoltcp-0.12.0/src/socket/
/// tcp.rs:2126`) recorded in [`Network::rebind_per_process_fields`]'s own doc comment.
///
/// The fix: back `socket_set` with a FIXED-CAPACITY slice allocated once (by whichever process
/// first constructs `Network`, i.e. exactly once per fork family) via
/// [`platform::SharedKernelStateProvider::shared_kernel_arena_alloc_bytes`], on a platform where
/// that reaches genuinely cross-process-shared memory at a fixed base address (see that method's
/// own doc comment) -- so the resulting `&'static mut [SocketStorage<'static>]`'s raw pointer
/// value is the SAME valid address in every attaching process, unlike a private-heap `Vec`
/// pointer. `smoltcp::iface::SocketSet`/`RingBuffer`/`PacketBuffer` are all backed by
/// `managed::ManagedSlice<'a, T>`, which supports exactly this "caller-supplied fixed slice"
/// shape via `SocketSet::new`/`RingBuffer::new`/`PacketBuffer::new`'s generic `Into<ManagedSlice>`
/// bound -- confirmed by reading smoltcp 0.12.0's own vendored source rather than assumed.
///
/// This fixes cross-process visibility of every INLINE field smoltcp stores per socket (protocol
/// state machine, sequence numbers, the `tuple` field from the panic above, `Meta`, ...), because
/// those now live directly in the shared bytes this slice points at. It does **not** yet fix
/// cross-process visibility of each socket's OWN rx/tx ring/packet buffer PAYLOAD bytes --
/// `tcp::Socket`/`udp::Socket` still construct those via `RingBuffer::new(vec![...])`/
/// `PacketBuffer::new(vec![...], vec![...])` (see [`Network::socket`]), so a socket's actual
/// data bytes remain private-heap-backed and only safely readable/writable from the process that
/// created that particular socket. Making the per-slot BUFFERS arena-native too is real,
/// separately-scoped follow-up work (needs a fixed-capacity buffer POOL indexed alongside
/// `socket_set`'s own slots and reused across a slot's socket lifecycle, since the arena
/// allocator underlying `shared_kernel_arena_alloc_bytes` is a bump allocator with no free list
/// and cannot absorb one allocation per ephemeral TCP connection) -- see
/// `docs/AGENTS_ARCHIVE_2026-09-17.md`.
///
/// `smoltcp::iface::SocketSet::add` PANICS if a `Borrowed` (fixed) `ManagedSlice` is full (see
/// smoltcp's own `socket_set.rs`), unlike the old unbounded `Vec`, so [`Network::socket`] checks
/// remaining capacity itself first and returns [`errors::SocketError::TooManySockets`] instead of
/// ever hitting that panic. 256 matches this codebase's other established fixed-slot caps
/// (`SharedUnixAddrPresenceTable`'s 256, `RawMutex::WaiterQueue`'s 32).
pub(crate) const MAX_SOCKETS: usize = 256;

/// How many listening ports can have a recorded queue owner at once ([`Network::listen_owner`]).
/// A desktop run arms ~13 listening ports (measured), so 64 leaves room; the table is a hint used
/// only to adopt an ORPHANED queue, so overflow degrades to today's behavior (no adoption) rather
/// than to anything worse -- no slot means "owner unknown", and an unknown owner is never assumed
/// dead, exactly like the pre-existing borrowed-listener path.
const LISTEN_OWNER_SLOTS: usize = 64;

mod socket_buffers;
use socket_buffers::SocketBuffers;

/// TCP connection timeout.
const TCP_CONNECT_TIMEOUT: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(75);

/// The `Network` provides access to all networking related functionality provided by LiteBox.
///
/// A LiteBox `Network` is parametric in the platform it runs on.
///
/// An important decision that must be made by a user of a `Network` is decided by
/// [`set_platform_interaction`](Self::set_platform_interaction), whose docs explain this further.
///
/// A user of `Network` who care about [events](crate::event) should call [set_socket_proxy](Self::set_socket_proxy)
/// to set up a proxy for each socket created, so that events can be notified properly.
/// A tag that is stable within one host process and differs between them, for diagnostics that run
/// from every process of the cross-process-fork family. The log carries no pid, so without a tag
/// "this process stopped ticking" and "this process keeps ticking but never reaches its own
/// listening entry" read identically in the file -- exactly the ambiguity chrF10 left behind
/// (selkies' 8081 heartbeat stopped at uptime 79.5s while other ports' ran to ~300s).
///
/// The address of a static is fixed for the life of a process and every host process is its own
/// image, so two processes report two addresses. The value is only ever printed.
fn host_process_tag() -> u64 {
    static TAG: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    static MARKER: u8 = 0;
    match TAG.load(core::sync::atomic::Ordering::Relaxed) {
        0 => {
            let v = &MARKER as *const u8 as usize as u64;
            TAG.store(v, core::sync::atomic::Ordering::Relaxed);
            v
        }
        t => t,
    }
}

pub struct Network<Platform>
where
    Platform: platform::IPInterfaceProvider
        + platform::TimeProvider
        + sync::RawSyncPrimitivesProvider
        + platform::SharedKernelStateProvider,
{
    litebox: LiteBox<Platform>,
    socket_set: smoltcp::iface::SocketSet<'static>,
    device: phy::Device<Platform>,
    interface: smoltcp::iface::Interface,
    /// Initial instant of creation, used as an arbitrary stop point from when time begins
    zero_time: Platform::Instant,
    // TODO: Maybe we should have separate allocators for TCP, UDP, ...?
    local_port_allocator: LocalPortAllocator,
    platform_interaction: PlatformInteraction,
    /// FDs that are queued for eventual closure. A fixed, pointer-free, `MAX_SOCKETS`-capacity
    /// array of slots (`None` == empty), NOT a `Vec` (as this used to be) -- same fix, same
    /// reason, as `closing_in_background` just below: `Network`, including this field inline
    /// within it, lives in the cross-process shared kernel arena, and a `Vec`'s backing buffer is
    /// a SEPARATE allocation on the constructing process's private heap, reachable only through a
    /// raw pointer stored inline in the `Vec` -- a cross-process-forked child that ATTACHES to
    /// (rather than constructs) the shared `GlobalState` reads that same pointer VALUE,
    /// meaningless in its own address space. This field was originally left as the one exception
    /// to the `closing_in_background` fix (2026-09-17) because it additionally touched
    /// `DescriptorTable::drain_entries_full_covered_by`'s `&mut Vec<TypedFd<_>>` signature --
    /// live-caught exactly as predicted (twenty-eighth pass): `TypedFd::as_usize().unwrap()`
    /// panicked on `None`, reading a foreign process's dangling `Vec` pointer as if it were this
    /// process's own storage, repeatedly, on a real webtop boot. Fixed by widening
    /// `drain_entries_full_covered_by` to take `&mut [Option<TypedFd<_>>]` instead (a fixed array
    /// coerces to that slice type for free), closing the gap this field's own prior doc comment
    /// had already named and deferred.
    queued_for_closure: [Option<SocketFd<Platform>>; MAX_SOCKETS],
    /// Sockets that are closing in the background. A fixed, pointer-free, `MAX_SOCKETS`-capacity
    /// array of slots (`None` == empty), NOT a `Vec` (as this used to be) -- `Network`, including
    /// this field inline within it, lives in the cross-process shared kernel arena
    /// (`GlobalState`'s `net: Mutex<Network<Platform>>`, placed via
    /// `SharedKernelStateProvider::create_shared_kernel_state`/`ShimGlobalState`). A `Vec`'s
    /// backing buffer is a SEPARATE allocation on the constructing process's private heap,
    /// reachable only through a raw pointer stored inline in the `Vec` -- a cross-process-forked
    /// child that ATTACHES to (rather than constructs) the shared `GlobalState` reads that same
    /// pointer VALUE, meaningless in its own address space, so `Vec::retain`/`push` read/write
    /// garbage as `SocketHandle`s. Confirmed live (2026-09-17): a forked child panicked
    /// `index out of bounds: the len is 256 but the index is 3414407380873671541` in
    /// `smoltcp::iface::socket_set::SocketSet::retain` (`litebox/src/net/local_ports.rs`'s
    /// `LocalPortAllocator::refcount` doc comment covers the identical bug class, found and fixed
    /// the same pass; `queued_for_closure` above got the same fix, twenty-eighth pass).
    closing_in_background: [Option<smoltcp::iface::SocketHandle>; MAX_SOCKETS],
    /// Smoltcp sockets that have a referent in ANOTHER process of the cross-process-fork family
    /// (recorded by [`Self::fork_adopt`]'s borrowed arms, which hand out exactly such second
    /// referents). Fixed, pointer-free and `MAX_SOCKETS`-sized for the same reason as
    /// `closing_in_background` just above.
    ///
    /// Set once per socket and never cleared: a mark means "some process other than the one that
    /// created this socket may be the one reading it", which stays true for as long as the socket
    /// is reachable, and a stale mark (a slot reused after a close) only costs an extra pull,
    /// never a lost byte -- see [`Self::drain_socket_channel_buffers`].
    shared_across_fork: [Option<smoltcp::iface::SocketHandle>; MAX_SOCKETS],
    /// Backlog slots already handed out by [`Self::accept`], so that no second process of the fork
    /// family hands the same connection out again. Fixed, pointer-free and `MAX_SOCKETS`-sized for
    /// the same reason as `closing_in_background` above.
    ///
    /// `fork()` shares a listening socket's open file description, so parent and child accept from
    /// ONE queue, and a connection in that queue belongs to whichever of them takes it first. The
    /// queue itself is the set of smoltcp slots armed on the listen endpoint -- shared already --
    /// but each process's `TcpServerSpecific::socket_set_handles` list is its own, so a slot is
    /// claimed here at `accept` time and skipped by every later scan. Cleared when a slot is armed
    /// back into LISTEN (a reused slot must be claimable again).
    accepted_slots: [Option<smoltcp::iface::SocketHandle>; MAX_SOCKETS],
    /// Which process is responsible for arming each listening port's accept queue, as
    /// `(port << 32) | host_pid` in one atomic word (`0` == slot free).
    ///
    /// A listening port's backlog slots live in the SHARED socket set, so they outlive the process
    /// that armed them -- and their only maintainer is that process's own tick, which
    /// `repair_listening_backlog` deliberately denies to a BORROWED referent (a borrower re-arming
    /// the port is chrF4's competing-queues bug). So when the owning process dies, the port keeps
    /// answering nothing forever: no process refills its backlog or reclaims its finished slots,
    /// and every fork child that inherited it sits on a deaf port. This is what lets a borrower
    /// tell "the owner is gone" (a pid that no longer exists) from "the owner is merely quiet",
    /// and take the queue over -- the one distinction a tag, an address or a counter cannot make.
    listen_owner: [core::sync::atomic::AtomicU64; LISTEN_OWNER_SLOTS],
    /// Storage for every socket's rx/tx buffers, placed in the shared kernel arena so any process
    /// in the fork family can poll any socket (see `socket_buffers`).
    buffers: SocketBuffers,
}

impl<Platform> Network<Platform>
where
    Platform: platform::IPInterfaceProvider
        + platform::TimeProvider
        + sync::RawSyncPrimitivesProvider
        + platform::SharedKernelStateProvider
        + platform::SystemInfoProvider,
{
    /// Construct a new `Network` instance
    ///
    /// This function is expected to only be invoked once per platform, as an initialization step,
    /// and the created `Network` handle is expected to be shared across all usage over the
    /// system.
    pub fn new(litebox: &LiteBox<Platform>) -> Self {
        let mut device = phy::Device::new(litebox.x.platform);
        let config = smoltcp::iface::Config::new(smoltcp::wire::HardwareAddress::Ip);
        let mut interface =
            smoltcp::iface::Interface::new(config, &mut device, smoltcp::time::Instant::ZERO);
        interface.update_ip_addrs(|ip_addrs| {
            match ip_addrs.push(smoltcp::wire::IpCidr::new(
                smoltcp::wire::IpAddress::Ipv4(INTERFACE_IP_ADDR),
                24,
            )) {
                Ok(()) => {}
                Err(_) => unreachable!(),
            }
            // Without this, `127.0.0.1` matches none of the interface's own
            // addresses, so smoltcp's route lookup falls through to the
            // default route and sends loopback traffic out to the real NAT
            // gateway instead of handing it straight to a local listening
            // socket -- the gateway then tries to open a REAL Windows socket
            // to `127.0.0.1`, which nothing is actually listening on (the
            // guest's own listening socket lives entirely inside this
            // process's smoltcp stack, never a real Windows socket), so the
            // connection just hangs until the guest's own connect timeout.
            match ip_addrs.push(smoltcp::wire::IpCidr::new(
                smoltcp::wire::IpAddress::Ipv4(Ipv4Addr::LOCALHOST),
                8,
            )) {
                Ok(()) => {}
                Err(_) => unreachable!(),
            }
        });
        match interface
            .routes_mut()
            .add_default_ipv4_route(GATEWAY_IP_ADDR)
        {
            Ok(None) => {}
            _ => unreachable!(),
        }
        Self {
            litebox: litebox.clone(),
            socket_set: smoltcp::iface::SocketSet::new(alloc_shared_socket_storage(
                litebox.x.platform,
            )),
            device,
            interface,
            zero_time: litebox.x.platform.now(),
            local_port_allocator: LocalPortAllocator::new(),
            platform_interaction: PlatformInteraction::Automatic,
            queued_for_closure: core::array::from_fn(|_| None),
            closing_in_background: [None; MAX_SOCKETS],
            shared_across_fork: [None; MAX_SOCKETS],
            accepted_slots: [None; MAX_SOCKETS],
            listen_owner: core::array::from_fn(|_| core::sync::atomic::AtomicU64::new(0)),
            buffers: SocketBuffers::new(litebox.x.platform),
        }
    }
}

/// Allocates [`MAX_SOCKETS`] worth of [`smoltcp::iface::SocketStorage`] slots via
/// [`platform::SharedKernelStateProvider::shared_kernel_arena_alloc_bytes`] and returns a
/// `'static` slice over them, all initialized to [`smoltcp::iface::SocketStorage::EMPTY`] --
/// see [`MAX_SOCKETS`]'s own doc comment for why this replaces the old `vec![]` (private-heap,
/// not genuinely cross-process-shared) backing for [`Network::socket_set`].
///
/// Called exactly once, from [`Network::new`] (itself expected to run exactly once per fork
/// family -- see that function's own doc comment), so there is no reuse/attach concern here:
/// unlike [`platform::SharedKernelStateProvider::create_shared_kernel_state`]/
/// `attach_shared_kernel_state`'s create-or-attach protocol, every OTHER process in the fork
/// family never calls this function at all -- it instead inherits the already-initialized
/// `Network` (this slice's raw pointer included) as part of attaching to the shared
/// `litebox_shim_linux::GlobalState` singleton that embeds it.
fn alloc_shared_socket_storage<Platform>(
    platform: &Platform,
) -> &'static mut [smoltcp::iface::SocketStorage<'static>]
where
    Platform: platform::SharedKernelStateProvider,
{
    let layout = core::alloc::Layout::array::<smoltcp::iface::SocketStorage<'static>>(MAX_SOCKETS)
        .expect("MAX_SOCKETS layout computation cannot overflow");
    let ptr = platform
        .shared_kernel_arena_alloc_bytes(layout)
        .expect("shared_kernel_arena_alloc_bytes failed for Network::socket_set (arena exhausted)")
        .cast::<smoltcp::iface::SocketStorage<'static>>();
    for i in 0..MAX_SOCKETS {
        // SAFETY: `ptr` names a freshly allocated, exclusively-owned (nothing else has a
        // reference to this allocation yet) region of at least `MAX_SOCKETS` uninitialized
        // `SocketStorage` slots, per `layout` above -- `ptr.add(i)` stays within that region for
        // every `i < MAX_SOCKETS`, and writing an `EMPTY` value into uninitialized memory (rather
        // than dropping any prior value) is exactly what a raw `write` is for.
        unsafe {
            ptr.add(i).write(smoltcp::iface::SocketStorage::EMPTY);
        }
    }
    // SAFETY: `ptr` is non-null and points to `MAX_SOCKETS` contiguous, now fully-initialized
    // `SocketStorage` values (the loop above), validly aligned per `layout`. The `'static`
    // lifetime is sound because this allocation is placed in the shared kernel arena (on a
    // platform where `shared_kernel_arena_alloc_bytes` reaches one) or the process's own leaked
    // global-allocator memory (the default implementation) -- either way, per
    // `SharedKernelStateProvider::shared_kernel_arena_alloc_bytes`'s own doc comment, this
    // allocation is NEVER reclaimed, exactly matching every other `'static` promotion this
    // codebase already performs over such arena memory (e.g. `phy::Device`'s own `&'static
    // Platform`). No other code holds a reference to this memory (it was just allocated), so
    // handing out an exclusive `&'static mut` is sound.
    unsafe { core::slice::from_raw_parts_mut(ptr.as_ptr(), MAX_SOCKETS) }
}

/// [`SocketHandle`] stores all relevant information for a specific [`SocketFd`], for easy access
/// from [`SocketFd`], _except_ the `Socket` itself which is stored in the [`Network::socket_set`].
pub(crate) struct SocketHandle<Platform: RawSyncPrimitivesProvider + TimeProvider> {
    /// Whether this socket handle is going away soon (i.e., `close` has been invoked upon it but
    /// it lingers for a bit to allow pending data to be sent).
    ///
    /// Atomic so `close` can flag it under a shared entry lock: a thread blocked in a read of the
    /// same socket holds that lock, and `close` must not wait for it.
    consider_closed: core::sync::atomic::AtomicBool,
    /// `shutdown(SHUT_WR)` was requested while written data had not yet reached the wire: the
    /// FIN is sent once it has, never ahead of it.
    shutdown_wr_pending: bool,
    /// The handle into the `socket_set`
    handle: smoltcp::iface::SocketHandle,
    specific: ProtocolSpecific,
    /// The proxy associated with this socket to enable lock-free data transfer
    /// and event notification
    proxy: Option<alloc::sync::Arc<NetworkProxy<Platform>>>,
    /// Whether this descriptor is a BORROWED second reference to a socket another process in the
    /// cross-process-fork family owns (`Network::fork_adopt`). Such a reference's `close()`
    /// releases only the reference: it must NOT close/abort the shared smoltcp socket or
    /// deallocate the shared local port, or the process that actually owns the socket loses a
    /// connection it still holds -- the Linux `fork()` semantics this exists to reproduce
    /// (`close()` in the child cannot tear down the parent's end).
    borrowed: bool,
    /// Whether `handle` names a smoltcp socket THIS process added to the shared socket set purely
    /// to name an endpoint it inherited, and that no other process has a referent to: `fork_adopt`'s
    /// `"L"`/`"u"` arms, which hand a fork child a second reference to a listening endpoint by
    /// creating a fresh socket for it (the port's backlog slots stay the parent's). Releasing that
    /// reference has to REMOVE the socket from the set, because it is not the parent's socket and
    /// nothing else can ever name it again -- left in place it costs one socket-table slot and two
    /// shared buffer-pool claims for the rest of the session, per adoption (xproc28: `sockets=24`
    /// -> `sockets=46` in a run whose arms each adopt one listener and free everything they open).
    ///
    /// False everywhere else: a socket the ordinary `closing_in_background` path retires
    /// (`socket()`, `accept`) or one another process still uses (`fork_adopt`'s `"T"`/`"U"`), for
    /// which removing it here would tear down a live connection.
    own_slot: bool,
}

impl<Platform: RawSyncPrimitivesProvider + TimeProvider> SocketHandle<Platform> {
    /// Convenience function to perform an operation depending on the socket type
    fn with_socket<TCP, UDP, R>(
        &self,
        socket_set: &smoltcp::iface::SocketSet<'static>,
        tcp: TCP,
        udp: UDP,
    ) -> R
    where
        TCP: FnOnce(&tcp::Socket) -> R,
        UDP: FnOnce(&udp::Socket) -> R,
    {
        match self.protocol() {
            crate::net::Protocol::Tcp => {
                let tcp_socket = socket_set.get::<tcp::Socket>(self.handle);
                tcp(tcp_socket)
            }
            crate::net::Protocol::Udp => {
                let udp_socket = socket_set.get::<udp::Socket>(self.handle);
                udp(udp_socket)
            }
            crate::net::Protocol::Icmp | crate::net::Protocol::Raw { protocol: _ } => {
                unimplemented!()
            }
        }
    }

    fn with_socket_mut<TCP, UDP, R>(
        &mut self,
        socket_set: &mut smoltcp::iface::SocketSet<'static>,
        tcp: TCP,
        udp: UDP,
    ) -> R
    where
        TCP: FnOnce(&mut tcp::Socket) -> R,
        UDP: FnOnce(&mut udp::Socket) -> R,
    {
        match self.protocol() {
            crate::net::Protocol::Tcp => {
                let tcp_socket = socket_set.get_mut::<tcp::Socket>(self.handle);
                tcp(tcp_socket)
            }
            crate::net::Protocol::Udp => {
                let udp_socket = socket_set.get_mut::<udp::Socket>(self.handle);
                udp(udp_socket)
            }
            crate::net::Protocol::Icmp | crate::net::Protocol::Raw { protocol: _ } => {
                unimplemented!()
            }
        }
    }
}

impl<Platform: RawSyncPrimitivesProvider + TimeProvider> core::ops::Deref
    for SocketHandle<Platform>
{
    type Target = ProtocolSpecific;
    fn deref(&self) -> &Self::Target {
        &self.specific
    }
}
impl<Platform: RawSyncPrimitivesProvider + TimeProvider> core::ops::DerefMut
    for SocketHandle<Platform>
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.specific
    }
}

#[expect(
    dead_code,
    reason = "these might eventually get used, they exist for completeness sake"
)]
pub(crate) enum ProtocolSpecific {
    Tcp(TcpSpecific),
    Udp(UdpSpecific),
    Icmp(IcmpSpecific),
    Raw(RawSpecific),
}

pub(crate) struct TcpSpecific {
    local_port: Option<LocalPort>,
    server_socket: Option<TcpServerSpecific>,
    /// Whether to immediately close the socket when closed (i.e., no graceful FIN handshake)
    immediate_close: AtomicBool,
    connect_initiated_at_us: Option<smoltcp::time::Instant>,
    /// The port this socket dialled, kept because smoltcp clears the socket's own endpoints once it
    /// closes: the sweep that later decides the connect's errno (see [`report_connect_outcome`])
    /// runs after that and would otherwise name every failure `port=0`.
    connect_peer_port: Option<u16>,
}

struct TcpServerSpecific {
    ip_listen_endpoint: smoltcp::wire::IpListenEndpoint,
    /// Specified backlog via `listen`, no packets can be `accept`ed unless this is `Some`
    backlog: Option<u16>,
    socket_set_handles: Vec<smoltcp::iface::SocketHandle>,
    /// Set while every slot of this port is spent (none left in LISTEN), so that state is reported
    /// once per episode instead of once per tick -- the sweep runs every tick, in every process
    /// of the fork family, so an unthrottled warning here would bury the log.
    no_slot_listening_reported: bool,
}

impl TcpServerSpecific {
    fn refill_to_backlog(
        &mut self,
        socket_set: &mut smoltcp::iface::SocketSet,
        buffers: &mut SocketBuffers,
    ) {
        let backlog = self.backlog.unwrap();
        for _ in self.socket_set_handles.len()..backlog.into() {
            // `socket_set` is fixed-capacity now (see `MAX_SOCKETS`'s own doc comment): stop
            // refilling the backlog early rather than let `SocketSet::add` panic when the whole
            // table happens to be full -- a smaller-than-requested accept backlog under real
            // resource pressure, not a crash, matches ordinary OS behavior under fd/socket
            // exhaustion.
            if socket_set.iter().count() >= MAX_SOCKETS {
                litebox_util_log::warn!(
                    port = self.ip_listen_endpoint.port;
                    "listen backlog cannot be refilled: the socket table is full"
                );
                break;
            }
            let Some((rx, tx, claim)) = buffers.tcp() else {
                report_exhausted_buffer_pool(socket_set, buffers, self.ip_listen_endpoint.port);
                break;
            };
            let mut listening_socket = tcp::Socket::new(rx, tx);
            match listening_socket.listen(self.ip_listen_endpoint) {
                Ok(()) => {}
                Err(tcp::ListenError::InvalidState) => {
                    // Impossible, because we _just_ created a new tcp::Socket, which begins
                    // in CLOSED state.
                    unreachable!()
                }
                Err(tcp::ListenError::Unaddressable) => {
                    // Impossible, since listen endpoint port is non 0.
                    unreachable!()
                }
            }
            let handle = socket_set.add(listening_socket);
            buffers.adopt(handle, claim);
            self.socket_set_handles.push(handle);
        }
    }
}

/// `(how many sockets the table holds`, `one `local:state:remote` token per socket in it)`.
///
/// `Network` is one object shared by every process of a cross-process-fork family, so the sockets
/// holding the table may belong to any of them, and a count alone cannot tell a leaked socket from
/// a busy one: only listing them answers "who is using all the slots".
fn socket_census(socket_set: &smoltcp::iface::SocketSet<'_>) -> (usize, alloc::string::String) {
    let mut census = alloc::string::String::new();
    let mut total = 0usize;
    for (_handle, socket) in socket_set.iter() {
        total += 1;
        match socket {
            smoltcp::socket::Socket::Tcp(t) => {
                let state = match t.state() {
                    tcp::State::Listen => "L",
                    tcp::State::SynReceived => "SR",
                    tcp::State::SynSent => "SS",
                    tcp::State::Established => "E",
                    tcp::State::FinWait1 => "FW1",
                    tcp::State::FinWait2 => "FW2",
                    tcp::State::CloseWait => "CW",
                    tcp::State::Closing => "CG",
                    tcp::State::LastAck => "LA",
                    tcp::State::TimeWait => "TW",
                    tcp::State::Closed => "C",
                };
                let local = t.local_endpoint().map_or(0, |e| e.port);
                let remote = t.remote_endpoint().map_or(0, |e| e.port);
                let _ = core::fmt::Write::write_fmt(&mut census, format_args!(" {local}:{state}:{remote}"));
            }
            smoltcp::socket::Socket::Udp(u) => {
                let local = u.endpoint().port;
                let _ = core::fmt::Write::write_fmt(&mut census, format_args!(" udp{local}"));
            }
            _ => {
                let _ = core::fmt::Write::write_fmt(&mut census, format_args!(" other"));
            }
        }
    }
    (total, census)
}

/// A `connect(2)` that did not complete -- refused, reset, timed out or unaddressable. The
/// application gets an errno and, before this line, the log said nothing at all, so a port that
/// had gone deaf was indistinguishable from one whose SYN never arrived (chrF8/chrF9: selkies'
/// 8081 streamed to the client it had already accepted while every new connect failed silently).
/// Throttled: browsers fail connects routinely, so an unthrottled line here buries the log. The
/// FIRST FOUR failures of a process are always reported - a probe that fails three connects on a
/// port it cares about would otherwise be invisible behind the 1/64 throttle (chrF15's three
/// HOLD-tick curls to 8081 left no line at all, while 19 sampled lines hid ~1216 real ones).
/// `SEEN` is shared by EVERY process in the family, so those first four are spent by whatever
/// fails earliest in the run; every failure is repeated UNTHROTTLED at `debug` with its `n`, which
/// is what a probe turns on to see its own (chrF19: the three refused curls to 8081 at t=90/120/150
/// left ZERO lines, because 12 earlier failures had already spent the budget - the absence of a
/// `diag-connect` line proves nothing).
fn report_connect_failure(what: &'static str, port: u16) {
    static SEEN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let n = SEEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    litebox_util_log::debug!(what:% = what, port = port, n = n; "diag-connect: connect(2) did not complete");
    if n >= 4 && n % 64 != 0 {
        return;
    }
    litebox_util_log::warn!(what:% = what, port = port; "diag-connect: connect(2) did not complete");
}

/// The same failure as [`report_connect_failure`], decided by the tick instead of by a second
/// `connect(2)`. A non-blocking connect parks its socket in `Connecting` and returns; it is this
/// sweep that later notices the socket died without ever completing, and it is the ONLY place that
/// decides such a connect's errno -- so while it logged nothing, a probe's refusal had no `what=`
/// anywhere in the log and no way to tell a peer's RST from a SYN that went unanswered. (chrF20:
/// 8081 refused at t=90/120/150/180 s while `diag-connect` held 101 lines and not one of them was
/// a failure -- every one was the `in-progress` of the first call.) `elapsed_us` against
/// `timeout_us` is that distinction: a refusal lands early, a timeout at or past the deadline.
/// `sockets` is how many sockets THIS process's socket set holds at that instant - the same count
/// `diag-port` prints as `sockets=`. chrF32 needed it: its refusals read `slots=none` while the
/// poller's own heartbeat read `port=8081 slots=8 listening=8` at the same wall clock, so either
/// the two processes hold DIFFERENT socket sets or one of them is not seeing the shared one.
fn report_connect_outcome(
    what: &'static str,
    port: u16,
    local: u16,
    elapsed_us: u64,
    timeout_us: u64,
    slots: &str,
    sockets: usize,
    state: &'static str,
    closed_here: bool,
) {
    static SEEN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let n = SEEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    litebox_util_log::debug!(
        tag = host_process_tag(),
        what:% = what,
        port = port,
        local = local,
        elapsed_us = elapsed_us,
        timeout_us = timeout_us,
        slots:% = slots,
        sockets = sockets,
        state:% = state,
        closed_here = closed_here,
        n = n;
        "diag-connect-outcome: a connect(2) that was left in progress ended without a connection"
    );
    if n >= 4 && n % 64 != 0 {
        return;
    }
    litebox_util_log::warn!(
        what:% = what,
        port = port,
        local = local,
        elapsed_us = elapsed_us,
        timeout_us = timeout_us,
        slots:% = slots,
        sockets = sockets,
        state:% = state,
        closed_here = closed_here;
        "diag-connect-outcome: a connect(2) that was left in progress ended without a connection"
    );
}

/// The states of every smoltcp socket bound to `port`, at the instant a connect to it was refused.
///
/// `diag-port` speaks only on every 512th sweep, so a refusal has never carried its own cause:
/// chrF19/20/21 read `slots=8 listening=8 pending=0` at some heartbeat while 8081 was refusing at
/// every tick, and a heartbeat and a refusal are whole sweeps and seconds apart -- a port that is
/// deaf most of the time still looks armed at the sample, so the two could not be reconciled. smoltcp
/// dispatches a SYN only to a socket in `Listen`, so that count IS the answer, and reading it here
/// ties it to the failure instead of to whatever the port looked like some sweeps later.
fn port_slots_at(socket_set: &smoltcp::iface::SocketSet<'_>, port: u16) -> alloc::string::String {
    let mut out = alloc::string::String::new();
    for (_handle, socket) in socket_set.iter() {
        let smoltcp::socket::Socket::Tcp(t) = socket else {
            continue;
        };
        if t.local_endpoint().map_or(0, |e| e.port) != port {
            continue;
        }
        let state = match t.state() {
            tcp::State::Listen => "L",
            tcp::State::SynReceived => "SR",
            tcp::State::SynSent => "SS",
            tcp::State::Established => "E",
            tcp::State::FinWait1 => "FW1",
            tcp::State::FinWait2 => "FW2",
            tcp::State::CloseWait => "CW",
            tcp::State::Closing => "CG",
            tcp::State::LastAck => "LA",
            tcp::State::TimeWait => "TW",
            tcp::State::Closed => "C",
        };
        let _ = core::fmt::Write::write_fmt(&mut out, format_args!("{state} "));
    }
    if out.is_empty() {
        out.push_str("none");
    }
    out
}

/// Says which sockets hold the buffer pool, when a backlog refill found none left.
///
/// `Network` is one object shared by every process of a cross-process-fork family, so the pool is
/// shared too: a listening port that cannot refill its backlog is deaf from then on no matter
/// which process polls it, and the sockets holding the slots may belong to any of them. Throttled
/// rather than per-tick -- the refill sweep runs every tick in every process, so an unthrottled
/// report here buries the log (measured: 144k lines in one run).
fn report_exhausted_buffer_pool(
    socket_set: &mut smoltcp::iface::SocketSet<'_>,
    buffers: &SocketBuffers,
    port: u16,
) {
    static SEEN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    if SEEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % 512 != 0 {
        return;
    }
    let (data_granted, data_used, meta_granted, meta_used, owners) = buffers.occupancy();
    let (total, census) = socket_census(socket_set);
    litebox_util_log::warn!(
        port = port,
        data_granted = data_granted,
        data_used = data_used,
        meta_granted = meta_granted,
        meta_used = meta_used,
        owners = owners,
        sockets = total,
        census:% = census;
        "diag-pool: a listening port cannot refill its backlog because the shared socket buffer pool has no slot left"
    );
}

pub(crate) struct UdpSpecific {
    /// Remote endpoint
    ///
    /// If `connect`-ed, this is the remote endpoint to which packets are sent by default.
    remote_endpoint: Option<smoltcp::wire::IpEndpoint>,
}
pub(crate) struct IcmpSpecific {}

pub(crate) struct RawSpecific {
    protocol: u8,
}

#[expect(
    dead_code,
    reason = "the dead ones exist for completeness sake, might eventually get used"
)]
impl ProtocolSpecific {
    fn protocol(&self) -> Protocol {
        match self {
            ProtocolSpecific::Tcp(_) => Protocol::Tcp,
            ProtocolSpecific::Udp(_) => Protocol::Udp,
            ProtocolSpecific::Icmp(_) => Protocol::Icmp,
            ProtocolSpecific::Raw(RawSpecific { protocol, .. }) => Protocol::Raw {
                protocol: *protocol,
            },
        }
    }

    /// Obtain a reference to the tcp-socket-specific data. Panics if non-TCP.
    fn tcp(&self) -> &TcpSpecific {
        match self {
            ProtocolSpecific::Tcp(specific) => specific,
            _ => unreachable!(),
        }
    }

    /// Obtain a mutable reference to the tcp-socket-specific data. Panics if non-TCP.
    fn tcp_mut(&mut self) -> &mut TcpSpecific {
        match self {
            ProtocolSpecific::Tcp(specific) => specific,
            _ => unreachable!(),
        }
    }

    /// Obtain a reference to the udp-socket-specific data. Panics if non-UDP.
    fn udp(&self) -> &UdpSpecific {
        match self {
            ProtocolSpecific::Udp(specific) => specific,
            _ => unreachable!(),
        }
    }

    /// Obtain a mutable reference to the udp-socket-specific data. Panics if non-UDP.
    fn udp_mut(&mut self) -> &mut UdpSpecific {
        match self {
            ProtocolSpecific::Udp(specific) => specific,
            _ => unreachable!(),
        }
    }

    /// Obtain a reference to the icmp-socket-specific data. Panics if non-ICMP.
    fn icmp(&self) -> &IcmpSpecific {
        match self {
            ProtocolSpecific::Icmp(specific) => specific,
            _ => unreachable!(),
        }
    }

    /// Obtain a mutable reference to the icmp-socket-specific data. Panics if non-ICMP.
    fn icmp_mut(&mut self) -> &mut IcmpSpecific {
        match self {
            ProtocolSpecific::Icmp(specific) => specific,
            _ => unreachable!(),
        }
    }

    /// Obtain a reference to the raw-socket-specific data. Panics if non-RAW.
    fn raw(&self) -> &RawSpecific {
        match self {
            ProtocolSpecific::Raw(specific) => specific,
            _ => unreachable!(),
        }
    }

    /// Obtain a mutable reference to the raw-socket-specific data. Panics if non-RAW.
    fn raw_mut(&mut self) -> &mut RawSpecific {
        match self {
            ProtocolSpecific::Raw(specific) => specific,
            _ => unreachable!(),
        }
    }
}

/// Whether [`Network::perform_platform_interaction`] needs to be manually invoked or not.
pub enum PlatformInteraction {
    /// Automatically (internally) invoked whenever any calls like `send`/`recv`/... are made.
    Automatic,
    /// Requires manually (periodically) invoking [`Network::perform_platform_interaction`]
    Manual,
}

#[derive(Clone, Copy)]
enum PollDirection {
    Ingress,
    Egress,
    Both,
}

/// Advice on when to invoke [`Network::perform_platform_interaction`] again.
///
/// It is perfectly ok to ignore this advice by calling things sooner (say, in a tight loop).
/// Specifically, it is harmless (but wastes energy) to call for interaction again sooner than
/// advised. In contrast, it _may_ be harmful (impacting quality of service) to call it later than
/// requested.
#[derive(Clone, Copy, Debug)]
pub enum PlatformInteractionReinvocationAdvice {
    /// It is likely helpful to call again immediately, without any delay. The function has returned
    /// control back to you to prevent unbounded length waits (crucial to prevent in
    /// non-pre-emptible environments), but otherwise has more work it anticipates it can do.
    CallAgainImmediately,
    /// You don't need to call again until more packets arrive on the device's receive side (i.e., `timeout` is `None`),
    /// or the given `timeout` expires.
    WaitOnDeviceOrSocketInteraction {
        timeout: Option<core::time::Duration>,
    },
}
impl PlatformInteractionReinvocationAdvice {
    #[must_use]
    pub fn call_again_immediately(self) -> bool {
        matches!(self, Self::CallAgainImmediately)
    }
}

impl<Platform> Network<Platform>
where
    Platform: platform::IPInterfaceProvider
        + platform::TimeProvider
        + sync::RawSyncPrimitivesProvider
        + platform::SharedKernelStateProvider
        + platform::SystemInfoProvider,
{
    /// Rebind every field of this `Network` that holds a raw, process-relative pointer captured at
    /// construction time to the CALLING process's own, always-correct equivalent.
    ///
    /// **UPDATE (2026-09-17, later same day still; `queued_for_closure` added 2026-09-2x,
    /// twenty-eighth pass): `socket_set`'s slot array, `closing_in_background`, and
    /// `queued_for_closure` are now all fixed, pointer-free, shared-arena-native storage -- see
    /// `Network::socket_set`'s `alloc_shared_socket_storage` and the `closing_in_background`/
    /// `queued_for_closure` fields' own doc comments for each fix and its live-caught evidence.
    /// `interface` (routes/neighbor-cache) remains the sole still-open instance of this doc
    /// comment's defect class.**
    ///
    /// `Network`'s smoltcp `socket_set`/`interface`/`closing_in_background`/`queued_for_closure`
    /// need to be genuinely shared across the whole cross-process-fork family for real guest
    /// behavior (nginx's own reverse proxy to selkies over `127.0.0.1:8081` -- see
    /// `docs/AGENTS_ARCHIVE_2026-09-17.md`'s working-browser-config section -- runs nginx and
    /// selkies as TWO SEPARATE cross-process-fork children; that loopback `connect()` can only
    /// resolve inside smoltcp's own virtual routing if both processes' smoltcp code walks the SAME
    /// socket set, so per-process-shadowing `Network`, unlike `futex_manager`, would silently
    /// break that specific already-working path rather than merely losing an optimization).
    ///
    /// **This claim was aspirational, not yet true of the implementation as of this note
    /// (2026-09-17, later same day as the `litebox`/`device` fixes below): `socket_set` is
    /// `smoltcp::iface::SocketSet::new(vec![])` -- an ordinary, growable, PRIVATE-per-process-heap
    /// `Vec` -- and `interface`/`queued_for_closure: Vec<SocketFd<_>>`/`closing_in_background:
    /// Vec<SocketHandle>` are the same shape. This is the SAME "GlobalState registry whose nodes
    /// live on the ordinary private heap" defect class as `pty_registry`/`flock_registry`/etc
    /// (this file's own "`SharedArc<T>`" section), just one level deeper (inside `Network`, which
    /// is itself correctly arena-placed) and not yet fixed for these four fields -- ONLY `litebox`/
    /// `device` below were. Live-caught 2026-09-17 (RUST_BACKTRACE=1, fourth pass after the
    /// pipe-handle-leak fix): a cross-process-fork child's smoltcp `tcp::Socket::dispatch` ->
    /// `seq_to_transmit` panicked `called Option::unwrap() on a None value` at
    /// `smoltcp-0.12.0/src/socket/tcp.rs:2126:46` (`self.tuple.unwrap()`) -- a `Socket`'s own
    /// `tuple` field reading back as `None` when a live connection's real state (in whichever
    /// process actually created it) has it `Some`, exactly the stale-shared-pointer symptom this
    /// whole doc comment otherwise describes. Recurred twice in one boot (non-fatal both times --
    /// the crashing fork child dies, the s6-style supervisor respawns, and the boot reached a new
    /// best point, `DE_LAUNCHED`, anyway) rather than being a hard blocker, which is why this is
    /// recorded here rather than rushed into an unverified fix: correctly making `socket_set`'s
    /// per-slot smoltcp `Socket`s (each with their own rx/tx ring buffers) shared-arena-native is a
    /// genuinely large, separate redesign (fixed socket-count cap, fixed-size arena-backed buffers
    /// per slot via smoltcp's own `Managed<'a, [u8]>` buffer constructors, same for `interface`'s
    /// routes/neighbor-cache storage), not a two-field rebind like `litebox`/`device` below -- see
    /// `docs/AGENTS_ARCHIVE_2026-09-17.md` for the live evidence and why a per-process-shadow
    /// (this struct's OTHER established fix pattern) is the WRONG pattern here specifically.
    ///
    /// Two of its OTHER fields are different in kind -- each is a raw pointer captured once by
    /// whichever process happened to construct `GlobalState`/`Network` first, then placed inline
    /// in the cross-process-shared arena; every OTHER process in the fork family, including every
    /// cross-process-fork child that ATTACHES to this same shared `Network` instance, inherits
    /// that first process's raw pointer bytes verbatim -- meaningless (and, once that first
    /// process has since exited, genuinely dangling) in its own address space:
    ///
    /// - `litebox: LiteBox<Platform>` exists only so this struct's own methods
    ///   (`close_pending_sockets`, `drain_all_socket_channel_buffers`, `bind`, `connect`, ...) can
    ///   reach `descriptor_table()`/`descriptor_table_mut()` -- and `Descriptors` is a PER-PROCESS
    ///   table (each process now constructs its own fresh, local `LiteBox` -- see this crate's own
    ///   `LiteBox::new` doc comment, "2026-09-17 create-vs-attach note", and
    ///   `litebox_shim_linux::GlobalStateHandle`'s doc comment for why `GlobalState` itself carries
    ///   no `litebox` field). Live-caught: a real `STATUS_ACCESS_VIOLATION` (0xc0000005) inside
    ///   `Descriptors::iter_mut`'s closure, reached via `close_pending_sockets`, on a plain `mkdir`
    ///   fork child with zero sockets of its own -- `close_pending_sockets`/
    ///   `drain_all_socket_channel_buffers` run as unconditional per-tick housekeeping over the
    ///   WHOLE shared `Network`, regardless of which process is currently holding the lock.
    /// - `device: phy::Device<Platform>` holds `platform: &'static Platform` (`phy::Device`'s own
    ///   field) -- the SAME defect one level deeper still, caught live IMMEDIATELY after the
    ///   `litebox` fix above landed: `STATUS_ACCESS_VIOLATION` inside
    ///   `litebox_platform_windows_userland::net::receive_ip_packet`, called through
    ///   `Device::receive`'s `self.platform.receive_ip_packet(...)`. `phy::Device` carries no other
    ///   state worth preserving across a rebind (`receive_buffer`/`send_buffer` are transient
    ///   smoltcp-poll-cycle scratch space, never held across a lock release), so it is cheaper and
    ///   safer to reconstruct it wholesale than to reach in and patch one field.
    ///
    /// The fix: every caller that locks `GlobalState.net` rebinds both fields to ITS OWN
    /// already-correct, per-process state (`GlobalStateHandle::net_lock`) before touching anything
    /// -- same shadow-field pattern as `litebox`/`proc_self_info`/`pts_registry`/`elf_patch_cache`/
    /// `exec_ranges_cache`/`segment_scan_cache`, applied one (for `litebox`) or two (for `device`)
    /// levels deeper because these particular stale pointers live inside a struct that is itself
    /// correctly, genuinely shared rather than at `GlobalState`'s own top level.
    pub fn rebind_per_process_fields(&mut self, litebox: &LiteBox<Platform>) {
        let loopback = core::mem::take(&mut self.device.loopback);
        self.device = phy::Device::new(litebox.x.platform);
        self.device.loopback = loopback;
        // 49th pass (2026-09-22), root-caused live via RUST_BACKTRACE=full on a reproducible
        // `buddy_system_allocator-0.11.0/src/lib.rs:165` "index out of bounds: the len is 34 but
        // the index is 53" panic, 3/3 independent occurrences with the BIT-IDENTICAL backtrace:
        // `net_worker (fork child)`'s first `perform_network_interaction()` call locks this shared
        // `Network`, calls this function, and a plain `self.litebox = litebox.clone()` here DROPS
        // the OLD `self.litebox` in place -- but `self` (this whole `Network`) lives in the
        // cross-process-SHARED kernel arena (this struct's own doc comment above), so the OLD
        // value's `Arc` inner pointer was captured by whichever process last called this function
        // (the parent, or a sibling fork child) and is meaningless, and possibly already-exited-
        // process-owned, raw bytes in THIS process's address space -- exactly the "stale-shared-
        // pointer" defect class this doc comment already describes for `litebox`/`device`, just
        // not yet applied to the ASSIGNMENT here that actually triggers a drop. An ordinary
        // `Arc::drop` on that foreign pointer walks into `Arc::drop_slow` (observed live: strong
        // count read as the foreign process's own transient value, hit what looks like zero) and
        // free the FD table's `Vec<Option<IndividualEntry>>` through THIS process's real global
        // allocator using a `Layout` reconstructed from foreign/unrelated memory -- corrupting
        // `buddy_system_allocator::Heap::free_list` with a garbage size class instead of
        // page-faulting cleanly, because the bytes happen to look like a plausible (if huge)
        // `RawVec` capacity rather than an invalid pointer. Same reasoning, same fix shape, as
        // `reset_after_poisoning`'s own `core::mem::forget` a few lines below (which fixed the
        // identical hazard for `SocketSet::remove`'s returned `Socket` -- see that function's own
        // doc comment) -- `mem::forget` the stale value instead of letting it drop normally: safe
        // because it is never reachable through `Network` again after this call, and because it
        // was never this process's allocation to free in the first place.
        let stale_litebox = core::mem::replace(&mut self.litebox, litebox.clone());
        core::mem::forget(stale_litebox);
    }

    /// Resets every mutable networking collection back to the same safe, empty starting point
    /// [`Self::new`] itself establishes -- `socket_set`, `closing_in_background`,
    /// `queued_for_closure`, and `local_port_allocator`'s refcount table -- WITHOUT reallocating
    /// `socket_set`'s shared-kernel-arena backing storage (that arena is an append-only bump
    /// allocator with no free list, see [`alloc_shared_socket_storage`]'s own doc comment, so
    /// re-running [`Self::new`] on every call here would slowly exhaust the bounded 64 MiB arena
    /// over a long-lived boot that hits this path repeatedly under a fork-heavy workload).
    ///
    /// # When to call this
    ///
    /// Exactly when a lock acquisition on the `Mutex<Platform, Network<Platform>>` wrapping this
    /// `Network` reports it was forced open by dead-holder recovery (`litebox::sync::Mutex::
    /// lock_recovering_poison`, itself backed by `platform::RawMutex::take_poison`) -- see
    /// `RawMutex`'s own `poisoned` field doc comment (`litebox_platform_windows_userland`) for the
    /// full defect class this exists to close: a lock holder that dies mid-mutation (a
    /// cross-process-fork child killed while its `net_worker` thread held this same lock, the
    /// concrete case that motivated this) leaves whichever of the fields above it was touching
    /// possibly torn -- e.g. a handle already removed from `socket_set` but still recorded in
    /// `closing_in_background`, or vice versa -- so a LATER, entirely unrelated caller can panic
    /// deep in `smoltcp` (`"handle does not refer to a valid socket"`) on a handle some field still
    /// names but whose backing slot something else already emptied. `SocketHandle` in this crate's
    /// `smoltcp` dependency (0.12) is a bare slot index with no generation counter (see
    /// `smoltcp::iface::socket_set::SocketSet`'s own source), so there is no cheaper way to tell a
    /// stale handle apart from a live one short of wiping every field that could hold one.
    ///
    /// # What this does NOT fix
    ///
    /// `interface`'s own smoltcp-internal state (ARP/route caches) is left untouched -- it holds no
    /// per-socket handles and is not implicated in this bug class. A [`SocketFd`]/[`LocalPort`]
    /// token some OTHER, still-alive process minted before this reset and continues to hold becomes
    /// stale the instant this runs (its handle/port no longer names anything real) -- using it
    /// afterward can still panic exactly as before. This is the accepted, disclosed trade-off: a
    /// real loss of in-flight connections for whatever was live at the moment of the crash, in
    /// exchange for every FUTURE caller of this `Network` (the vast majority, since a `socket`/
    /// `bind`/`connect` sequence completes in microseconds relative to `LIVENESS_CHECK_INTERVAL`)
    /// seeing self-consistent state instead of inheriting the crash's own torn snapshot forever.
    pub fn reset_after_poisoning(&mut self) {
        let stale_handles: Vec<smoltcp::iface::SocketHandle> =
            self.socket_set.iter().map(|(handle, _)| handle).collect();
        for handle in stale_handles {
            // Live-caught (2026-09-21): dead-holder detection itself is not exclusive across
            // processes racing to acquire this same poisoned lock -- two separate cross-process
            // children can each independently observe "recorded holder process is dead" within
            // the same tick (confirmed live: two DIFFERENT per-process elapsed-time clocks, see
            // this module's own `init_logging()` warning, both logged that exact message within
            // the same wall-clock window) and both proceed to call this function concurrently
            // against the SAME shared `Network`. Whichever wins the race removes `handle` first;
            // without this check the loser's own `stale_handles` snapshot (collected from the
            // SAME `socket_set` before either side touched it) still names it, and calling
            // `remove` a second time panics exactly like any other stale-handle call site this
            // module already guards. `socket_set_contains` is the same guard `remove_dead_
            // sockets`/`close_pending_sockets`/`drain_socket_channel_buffers` already use.
            if !Self::socket_set_contains(&self.socket_set, handle) {
                continue;
            }
            // `SocketSet::remove` clears the slot back to its `SocketStorage::EMPTY` starting
            // state and hands back the `Socket` value -- `core::mem::forget`, deliberately NOT a
            // normal drop, is the whole point here, and was itself a real, live-caught bug the
            // first version of this function shipped with: a `tcp`/`udp`/`icmp` socket's own RX/TX
            // ring buffers (`vec![0u8; SOCKET_BUFFER_SIZE]` at creation time, `Network::socket`)
            // are ordinary private-per-process-heap allocations, made by WHICHEVER process
            // originally called `socket()` for that handle -- the exact same possibly-dead process
            // this whole reset exists to recover from. Letting the returned `Socket` drop normally
            // runs its buffers' `Vec` destructor, which calls THIS process's global allocator
            // (`buddy_system_allocator::LockedHeapWithRescue::dealloc`) on a pointer that names
            // memory in a DIFFERENT process's (possibly already-exited) address space -- live
            // `cdb`-confirmed: a thread frozen indefinitely inside exactly that `dealloc` call,
            // reached from this function via `smoltcp::socket::Socket`'s drop glue, immediately
            // after the first version of this fix landed. `mem::forget` intentionally abandons
            // that (already being discarded, per this whole function's own contract) buffer memory
            // instead of touching it -- safe and correct because it is never reachable through
            // `Network` again after this loop, and because it was never THIS process's allocation
            // to free in the first place: Windows itself reclaims it in bulk, for free, whenever
            // the process that actually owns that address space exits.
            core::mem::forget(Self::remove_socket(&mut self.socket_set, &mut self.buffers, handle));
        }
        self.closing_in_background = [None; MAX_SOCKETS];
        self.buffers.reset();
        // Plain reassignment (not `mem::forget`-guarded like `socket_set` above) is safe here:
        // `TypedFd`'s `OwnedFd` holds no heap allocation at all (a bare `u32` + `AtomicBool`), so
        // dropping a queued-but-not-yet-closed one costs nothing and frees nothing in any
        // process's address space -- its `Drop` impl only ever panics when the
        // `panic_on_unclosed_fd_drop` Cargo feature is compiled in, which no runner in this
        // workspace currently enables.
        self.queued_for_closure = core::array::from_fn(|_| None);
        self.local_port_allocator.reset_after_poisoning();
    }

    /// Sets the interaction with the outside world to `platform_interaction`.
    ///
    /// If this is set to automatic, then a user of the network does not need to worry about
    /// scheduling or calling [`perform_platform_interaction`](Self::perform_platform_interaction).
    /// However, this may reduce predictability in terms of how quickly LiteBox responds to calls,
    /// since any network calls may incur non-trivial performance penalty.
    ///
    /// On the other hand, more performance can be had in scenarios that can support (say) a
    /// separate thread that repeatedly invokes
    /// [`perform_platform_interaction`](Self::perform_platform_interaction), or in scenarios where
    /// the user wants greater control over _when_ processing is performed, if done synchronously.
    ///
    /// By default, for convenience, the default setting (if this function is not invoked) is
    /// [`PlatformInteraction::Automatic`].
    pub fn set_platform_interaction(&mut self, platform_interaction: PlatformInteraction) {
        self.platform_interaction = platform_interaction;
    }

    /// Performs queued interactions with the outside world.
    ///
    /// # Panics
    ///
    /// This function panics if run without first using [`Self::set_platform_interaction`] to set
    /// interactions to manual.
    pub fn perform_platform_interaction(&mut self) -> PlatformInteractionReinvocationAdvice {
        assert!(
            matches!(self.platform_interaction, PlatformInteraction::Manual),
            "Requires manual-mode interactions"
        );
        match self.internal_perform_platform_interaction() {
            smoltcp::iface::PollResult::SocketStateChanged => {
                PlatformInteractionReinvocationAdvice::CallAgainImmediately
            }
            smoltcp::iface::PollResult::None => {
                let poll_at = self.poll_at();
                PlatformInteractionReinvocationAdvice::WaitOnDeviceOrSocketInteraction {
                    timeout: poll_at,
                }
            }
        }
    }

    /// Return a _soft timeout_ (duration to wait) before calling [`Self::perform_platform_interaction`] again.
    ///
    /// Returns `None` if there is no pending timeout (i.e., no scheduled work requiring network operations).
    fn poll_at(&mut self) -> Option<core::time::Duration> {
        let timestamp = self.now();
        self.interface
            .poll_at(timestamp, &self.socket_set)
            .map(|instant| {
                if timestamp < instant {
                    let diff = instant - timestamp;
                    diff.into()
                } else {
                    core::time::Duration::ZERO
                }
            })
    }

    /// (Internal-only API) Actually perform the queued interactions with the outside world.
    fn internal_perform_platform_interaction(&mut self) -> smoltcp::iface::PollResult {
        self.attempt_to_close_queued();
        self.remove_dead_sockets();
        self.close_pending_sockets();

        // Drain all socket channel buffers before polling to ensure data flows
        self.drain_all_socket_channel_buffers();
        if !platform::IPInterfaceProvider::owns_ip_interface(self.device.platform) {
            // The owner process polls for everyone: this process's packet queue is not connected
            // to the gateway, so a poll here would lose the frames it transmits.
            return smoltcp::iface::PollResult::None;
        }
        self.interface
            .poll(self.now(), &mut self.device, &mut self.socket_set)
    }

    /// (Internal-only API) Perform the queued interactions.
    fn automated_platform_interaction(&mut self, _direction: PollDirection) {
        match self.platform_interaction {
            PlatformInteraction::Automatic => {
                self.internal_perform_platform_interaction();
            }
            PlatformInteraction::Manual => {}
        }
    }

    /// Remove dead sockets that were closing in the background
    ///
    /// `closing_in_background` is genuinely cross-process-shared (see its own field doc comment):
    /// `close_handle` lets ANY process's socket land here, and this function runs as unconditional
    /// per-tick housekeeping from EVERY process's own `internal_perform_platform_interaction`
    /// (`GlobalStateHandle::net_lock`'s doc comment), specifically so a socket one process closed
    /// still gets reaped once its FIN/RST sequence finishes even if that process has since exited.
    /// That means the `Socket` this reaps was very often created (and its RX/TX ring buffers
    /// heap-allocated, `Network::socket`'s `vec![0u8; SOCKET_BUFFER_SIZE]`) by a DIFFERENT process
    /// than whichever one's tick happens to observe it as ready here -- `core::mem::forget`
    /// (instead of a normal drop) is required for the exact same reason
    /// [`Self::reset_after_poisoning`] needs it: a private-per-process-heap buffer pointer is
    /// meaningless (or dangling, if the creating process already exited) in the reaping process's
    /// own address space, and running its `Vec` destructor through THIS process's global allocator
    /// hangs indefinitely inside `dealloc` -- live `cdb`-confirmed, a thread frozen inside exactly
    /// that call, reached from here, before this fix.
    /// Whether `handle` still names a live socket in `self.socket_set` -- see
    /// [`Self::reset_after_poisoning`]'s own doc comment for why a per-process descriptor's
    /// `SocketHandle` can survive a dead-holder-triggered reset stale: that reset only wipes
    /// `Network`'s own shared registries, never any process's own descriptor table, and
    /// smoltcp's `SocketHandle` (0.12) carries no generation counter to tell a stale one apart
    /// from a live one short of a linear scan. Bounded by `MAX_SOCKETS` (256), so this scan is
    /// cheap; guards every call site that would otherwise panic (`"handle does not refer to a
    /// valid socket"`, `smoltcp::iface::socket_set::SocketSet::get`/`get_mut`) on a handle
    /// something else already removed. Live-confirmed load-bearing 2026-09-18: without this,
    /// the exact same stale handle re-panicked every single tick even after the caller caught
    /// and recovered from the first panic (`reset_after_poisoning` alone does not stop a
    /// process's own still-open socket fd from repeatedly re-touching its own now-dead handle).
    fn socket_set_contains(
        socket_set: &smoltcp::iface::SocketSet<'static>,
        handle: smoltcp::iface::SocketHandle,
    ) -> bool {
        socket_set.iter().any(|(h, _)| h == handle)
    }

    /// Removes `handle` from the socket table and returns its buffer slots to the shared pools.
    fn remove_socket(
        socket_set: &mut smoltcp::iface::SocketSet<'static>,
        buffers: &mut SocketBuffers,
        handle: smoltcp::iface::SocketHandle,
    ) -> smoltcp::socket::Socket<'static> {
        let socket = socket_set.remove(handle);
        buffers.release(handle);
        socket
    }

    /// Drops every backlog slot of one listening port that reached a terminal TCP state, returning
    /// how many went.
    ///
    /// A backlog slot has no application fd pointing at it -- it becomes one only once `accept`
    /// hands it out -- so a slot sitting in a state `accept` never hands out is unreachable: no
    /// application call can ever move it on, and the only thing it still does is occupy one of the
    /// port's `backlog` slots, which smoltcp needs a slot in `Listen` to answer a SYN with. Left
    /// alone, `backlog` such connections make the port refuse every later SYN for the rest of the
    /// session. The socket's own buffers go back to the shared pools here, which is what lets the
    /// caller's `refill_to_backlog` re-arm the slot as a fresh listener.
    fn reclaim_finished_backlog_slots(
        socket_set: &mut smoltcp::iface::SocketSet<'static>,
        buffers: &mut SocketBuffers,
        handles: &mut Vec<smoltcp::iface::SocketHandle>,
    ) -> usize {
        let mut reclaimed = 0usize;
        handles.retain(|&handle| {
            if !Self::socket_set_contains(socket_set, handle) {
                return false;
            }
            let socket: &tcp::Socket = socket_set.get(handle);
            let terminal = matches!(
                socket.state(),
                tcp::State::Closed
                    | tcp::State::TimeWait
                    | tcp::State::Closing
                    | tcp::State::LastAck
                    | tcp::State::FinWait1
                    | tcp::State::FinWait2
            );
            // A `Closed` slot that still names a remote endpoint owes that peer an RST; let it send
            // it first, exactly as `remove_dead_sockets` does.
            if !terminal
                || (socket.state() == tcp::State::Closed && socket.remote_endpoint().is_some())
            {
                return true;
            }
            // `remove_socket` hands the socket back by value; it is dropped nowhere here, matching
            // `remove_dead_sockets` -- see that call's own comment on running a socket's destructor
            // from a process that did not create it.
            core::mem::forget(Self::remove_socket(socket_set, buffers, handle));
            reclaimed += 1;
            false
        });
        reclaimed
    }

    /// The recorded owner of `port`'s accept queue, if this port has one.
    fn listen_owner_of(
        listen_owner: &[core::sync::atomic::AtomicU64; LISTEN_OWNER_SLOTS],
        port: u16,
    ) -> Option<u32> {
        listen_owner.iter().find_map(|slot| {
            let packed = slot.load(core::sync::atomic::Ordering::Relaxed);
            (packed >> 32 == u64::from(port) && packed != 0).then_some(packed as u32)
        })
    }

    /// Records `pid` as the process responsible for arming `port`'s accept queue.
    fn record_listen_owner(
        listen_owner: &[core::sync::atomic::AtomicU64; LISTEN_OWNER_SLOTS],
        port: u16,
        pid: u32,
    ) {
        if pid == 0 {
            return;
        }
        let want = (u64::from(port) << 32) | u64::from(pid);
        for slot in listen_owner {
            let packed = slot.load(core::sync::atomic::Ordering::Relaxed);
            if packed == 0 {
                if slot
                    .compare_exchange(
                        0,
                        want,
                        core::sync::atomic::Ordering::Relaxed,
                        core::sync::atomic::Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    return;
                }
            } else if packed >> 32 == u64::from(port) {
                // A re-bind, possibly by another process: the armer is whoever listened last. A
                // pid left stale here would have every borrower conclude the live owner is dead.
                slot.store(want, core::sync::atomic::Ordering::Relaxed);
                return;
            }
        }
    }

    /// Installs `me` as `port`'s queue owner, but only while `dead` is still the recorded one --
    /// the one word that stops two borrowers from both adopting the queue and arming two
    /// competing backlogs on a single endpoint (chrF4's measured symptom).
    fn claim_listen_owner(
        listen_owner: &[core::sync::atomic::AtomicU64; LISTEN_OWNER_SLOTS],
        port: u16,
        dead: u32,
        me: u32,
    ) -> bool {
        if me == 0 || dead == 0 || me == dead {
            return false;
        }
        let want = (u64::from(port) << 32) | u64::from(me);
        listen_owner.iter().any(|slot| {
            let packed = slot.load(core::sync::atomic::Ordering::Relaxed);
            packed >> 32 == u64::from(port)
                && packed as u32 == dead
                && slot
                    .compare_exchange(
                        packed,
                        want,
                        core::sync::atomic::Ordering::Acquire,
                        core::sync::atomic::Ordering::Relaxed,
                    )
                    .is_ok()
        })
    }

    /// Whether a BORROWED referent of listening port `port` may take that port's accept queue
    /// over: only when the recorded owner is a pid that no longer exists. A live owner, an unknown
    /// owner, a port with a socket already in LISTEN, or a lost race with another borrower all
    /// leave this referent a borrower.
    fn promote_borrowed_listener(
        listen_owner: &[core::sync::atomic::AtomicU64; LISTEN_OWNER_SLOTS],
        socket_set: &mut smoltcp::iface::SocketSet<'static>,
        handles: &mut alloc::vec::Vec<smoltcp::iface::SocketHandle>,
        port: u16,
        platform: &Platform,
    ) -> bool {
        let me = platform.current_pid();
        let Some(owner) = Self::listen_owner_of(listen_owner, port) else {
            return false;
        };
        if owner == me {
            return true;
        }
        // Confirmed dead, never "quiet": a borrower that adopts a queue its owner is still
        // ticking for arms a second backlog beside the live one.
        if platform.is_process_alive(owner) {
            return false;
        }
        if !Self::claim_listen_owner(listen_owner, port, owner, me) {
            return false;
        }
        // Adopt every socket already armed on this endpoint before the caller refills: refill only
        // tops the list up to `backlog`, so leaving the dead owner's slots unlisted would add a
        // second queue of `backlog` sockets beside them -- the competing-queues shape this
        // promotion exists to prevent, reached from the other side.
        for (handle, socket) in socket_set.iter() {
            let smoltcp::socket::Socket::Tcp(socket) = socket else {
                continue;
            };
            if socket
                .local_endpoint()
                .is_some_and(|local| local.port == port)
                && !handles.contains(&handle)
            {
                handles.push(handle);
            }
        }
        static PROMOTED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        if PROMOTED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % 64 == 0 {
            litebox_util_log::warn!(
                port = port,
                dead_owner = owner,
                new_owner = me,
                adopted = handles.len();
                "diag-listener: adopted an orphaned listening port's queue (its owner is gone)"
            );
        }
        true
    }

    fn remove_dead_sockets(&mut self) {
        for slot in &mut self.closing_in_background {
            let Some(handle) = *slot else { continue };
            if !Self::socket_set_contains(&self.socket_set, handle) {
                *slot = None;
                continue;
            }
            let tcp_socket = self.socket_set.get::<tcp::Socket>(handle);
            // a socket in the CLOSED state with the remote endpoint set means that an outgoing RST packet is pending
            if !tcp_socket.is_open() && tcp_socket.remote_endpoint().is_none() {
                core::mem::forget(Self::remove_socket(&mut self.socket_set, &mut self.buffers, handle));
                *slot = None;
            }
        }
    }

    /// Close all finished sockets that are marked as closed but waiting for pending data to be sent
    fn close_pending_sockets(&mut self) {
        // Best-effort for the same reason `attempt_to_close_queued` is: this runs with the
        // cross-process `net_lock` held, so parking on the descriptor table here would let one
        // guest thread that holds the table and wants `net_lock` freeze every process.
        let Some(table) = self.litebox.try_descriptor_table() else {
            return;
        };
        for (_, mut handle) in table.iter_mut_nowait::<Network<Platform>>() {
            let socket_handle = &mut handle.entry;
            if socket_handle.shutdown_wr_pending {
                if !Self::socket_set_contains(&self.socket_set, socket_handle.handle) {
                    socket_handle.shutdown_wr_pending = false;
                } else if !socket_handle
                    .proxy
                    .as_ref()
                    .is_some_and(|proxy| proxy.has_pending_tx())
                {
                    let sent_fin = socket_handle.with_socket_mut(
                        &mut self.socket_set,
                        |tcp_socket| {
                            let has_pending_data =
                                tcp_socket.may_send() && tcp_socket.send_queue() > 0;
                            if !has_pending_data {
                                tcp_socket.close();
                            }
                            !has_pending_data
                        },
                        |_| true,
                    );
                    if sent_fin {
                        socket_handle.shutdown_wr_pending = false;
                    }
                }
            }
            if socket_handle
                .consider_closed
                .load(core::sync::atomic::Ordering::Relaxed)
            {
                // See `socket_set_contains`'s doc comment: a stale handle (this descriptor's own
                // socket, wiped out from under it by a dead-holder `reset_after_poisoning()`
                // elsewhere) has nothing left to close.
                if !Self::socket_set_contains(&self.socket_set, socket_handle.handle) {
                    socket_handle
                        .consider_closed
                        .store(false, core::sync::atomic::Ordering::Relaxed);
                    continue;
                }
                if let Some(proxy) = &socket_handle.proxy
                    && proxy.has_pending_tx()
                {
                    continue;
                }

                let closed = socket_handle.with_socket_mut(
                    &mut self.socket_set,
                    |tcp_socket| {
                        let has_pending_data = tcp_socket.may_send() && tcp_socket.send_queue() > 0;
                        if !has_pending_data {
                            tcp_socket.close();
                        }
                        !has_pending_data
                    },
                    |udp_socket| {
                        let has_pending_data = udp_socket.is_open() && udp_socket.send_queue() > 0;
                        if !has_pending_data {
                            udp_socket.close();
                        }
                        !has_pending_data
                    },
                );
                if closed {
                    socket_handle
                        .consider_closed
                        .store(false, core::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }

    /// Record that `handle` is reachable from another process of the fork family as well (a
    /// `fork_adopt` borrowed arm just handed out such a second referent).
    fn mark_shared_across_fork(&mut self, handle: smoltcp::iface::SocketHandle) {
        if self.is_shared_across_fork(handle) {
            return;
        }
        if let Some(slot) = self.shared_across_fork.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(handle);
        }
    }

    fn is_shared_across_fork(&self, handle: smoltcp::iface::SocketHandle) -> bool {
        self.shared_across_fork
            .iter()
            .any(|marked| *marked == Some(handle))
    }

    /// Records that `handle` -- a backlog slot of some listening port -- has been handed out by
    /// `accept`, so no other process of the fork family hands the same connection out again.
    /// These four take the array itself rather than `&self` because the one caller that matters,
    /// [`Self::accept`], holds a borrow of `self.litebox` (the descriptor table) across them; the
    /// array and `socket_set` are disjoint fields, so naming them keeps that borrow legal.
    fn mark_accepted_in(
        accepted_slots: &mut [Option<smoltcp::iface::SocketHandle>; MAX_SOCKETS],
        handle: smoltcp::iface::SocketHandle,
    ) {
        if Self::is_accepted_in(accepted_slots, handle) {
            return;
        }
        if let Some(slot) = accepted_slots.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(handle);
        }
    }

    fn is_accepted_in(
        accepted_slots: &[Option<smoltcp::iface::SocketHandle>; MAX_SOCKETS],
        handle: smoltcp::iface::SocketHandle,
    ) -> bool {
        accepted_slots
            .iter()
            .any(|marked| *marked == Some(handle))
    }

    /// Forgets the claim on `handle`, for a slot armed back into LISTEN: the same smoltcp slot
    /// index gets reused for the port's next connection, which must be claimable again.
    fn clear_accepted_in(
        accepted_slots: &mut [Option<smoltcp::iface::SocketHandle>; MAX_SOCKETS],
        handle: smoltcp::iface::SocketHandle,
    ) {
        for slot in accepted_slots.iter_mut() {
            if *slot == Some(handle) {
                *slot = None;
            }
        }
    }

    /// A connection queued for `port` that no process has accepted yet: an established slot whose
    /// local endpoint is the listen port (a slot still in LISTEN has no remote endpoint at all,
    /// and one already accepted is marked in [`Self::accepted_slots`]).
    ///
    /// This is what makes a listening socket shared across `fork()`: the queue is the shared set
    /// of smoltcp slots armed on the endpoint, not any one process's
    /// `TcpServerSpecific::socket_set_handles`, so a fork child -- whose own list is empty, because
    /// it must not arm a SECOND listener competing for the same SYNs -- accepts from the queue its
    /// parent owns, exactly as Linux hands one connection to whichever of them calls `accept`
    /// first.
    fn unclaimed_connection_on(
        socket_set: &smoltcp::iface::SocketSet<'static>,
        accepted_slots: &[Option<smoltcp::iface::SocketHandle>; MAX_SOCKETS],
        port: u16,
    ) -> Option<smoltcp::iface::SocketHandle> {
        socket_set.iter().find_map(|(handle, socket)| {
            let smoltcp::socket::Socket::Tcp(socket) = socket else {
                return None;
            };
            let established = matches!(
                socket.state(),
                tcp::State::Established | tcp::State::CloseWait
            );
            let on_port = socket
                .local_endpoint()
                .is_some_and(|local| local.port == port);
            let claimed = accepted_slots.iter().any(|marked| *marked == Some(handle));
            (established && on_port && socket.remote_endpoint().is_some() && !claimed)
                .then_some(handle)
        })
    }

    /// Forgets every claim whose slot is no longer an unaccepted connection -- a slot is handed
    /// out once and then either dies with its connection or is armed back into LISTEN for the
    /// port's next one, and smoltcp reuses a freed slot index, so the mark cannot simply stay.
    fn reap_stale_claims_in(
        socket_set: &smoltcp::iface::SocketSet<'static>,
        accepted_slots: &mut [Option<smoltcp::iface::SocketHandle>; MAX_SOCKETS],
    ) {
        let stale: alloc::vec::Vec<smoltcp::iface::SocketHandle> = accepted_slots
            .iter()
            .flatten()
            .copied()
            .filter(|handle| {
                !socket_set.iter().any(|(live, socket)| {
                    live == *handle
                        && matches!(socket, smoltcp::socket::Socket::Tcp(socket) if matches!(
                            socket.state(),
                            tcp::State::Established | tcp::State::CloseWait
                        ))
                })
            })
            .collect();
        for handle in stale {
            Self::clear_accepted_in(accepted_slots, handle);
        }
    }

    fn drain_all_socket_channel_buffers(&mut self) {
        let now = self.now();
        // Stale accept claims would suppress BOTH the readiness sweep below and `accept` itself
        // (see `Network::reap_stale_claims`), and this is the one place that runs for every
        // process on every tick whether or not anything is accepted.
        Self::reap_stale_claims_in(&self.socket_set, &mut self.accepted_slots);
        // Best-effort, same reason as `close_pending_sockets`: `net_lock` is held by the caller.
        // A table that stays unreadable tick after tick makes this whole sweep a no-op in one
        // process while every other process keeps going, which is indistinguishable in the log
        // from "this process stopped ticking" unless it is said out loud -- the ambiguity that
        // left chrF10/chrF11 unexplained (8081's heartbeat stopped at uptime 53.8s while 8082's
        // and 9222's ran to ~600s).
        let Some(table) = self.litebox.try_descriptor_table() else {
            static LOCKED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            if LOCKED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % 512 == 0 {
                litebox_util_log::warn!(
                    tag = host_process_tag();
                    "diag-tick: this process's descriptor table was locked, no socket was swept"
                );
            }
            return;
        };
        // This process's own descriptor table, whose address is per-process (every process has its
        // own, even though the socket set it names is shared). `host_process_tag()` -- the address
        // of a static -- came out IDENTICAL for every host process, because they are all the same
        // image mapped at the same base, so it cannot tell one process's tick from another's.
        let dtag = &*table as *const _ as usize as u64;
        let mut drain_seen = 0usize;
        let mut drain_ports: alloc::vec::Vec<u16> = alloc::vec::Vec::new();
        for (_, entry) in table.iter_nowait::<Network<Platform>>() {
            drain_seen += 1;
            if let ProtocolSpecific::Tcp(tcp_specific) = &entry.entry.specific {
                if let Some(server_socket) = tcp_specific.server_socket.as_ref() {
                    drain_ports.push(server_socket.ip_listen_endpoint.port);
                }
            }
            let shared_across_fork = self.is_shared_across_fork(entry.entry.handle);
            Self::drain_socket_channel_buffers(
                &mut self.socket_set,
                &entry.entry,
                now,
                shared_across_fork,
                false,
                &self.accepted_slots,
            );
        }
        // Separate pass: the repair needs to mutate each entry, and the drain above deliberately
        // takes a shared guard so a guest thread blocked in `read()` on a socket cannot starve
        // its own bytes. A contended entry is skipped here and caught by the next tick.
        let mut reached_ports: alloc::vec::Vec<u16> = alloc::vec::Vec::new();
        let mut repair_seen = 0usize;
        for (_, mut entry) in table.iter_mut_nowait::<Network<Platform>>() {
            repair_seen += 1;
            if let ProtocolSpecific::Tcp(tcp_specific) = &entry.entry.specific {
                if let Some(server_socket) = tcp_specific.server_socket.as_ref() {
                    reached_ports.push(server_socket.ip_listen_endpoint.port);
                }
            }
            Self::repair_listening_backlog(
                &self.listen_owner,
                &mut self.socket_set,
                &mut self.buffers,
                &mut entry.entry,
                self.litebox.platform(),
            );
        }
        // WHICH ports this tick reached, not just what it found: `iter_mut_nowait` skips any entry
        // a guest thread is holding, and a listening entry skipped on EVERY tick is never re-armed
        // -- a port that then goes deaf stays deaf while the connections it already accepted go on
        // streaming. chrF10 measured exactly that shape: in-guest connects to selkies' 8081 were
        // refused from t=30s to the end of the run while an idle guest port (8082) beside it kept
        // answering 200 at the same instants, selkies kept encoding at 14 FPS to the end (so its
        // process was alive), and 8081's OWN repair heartbeat stopped at uptime 79.5s while 8082's
        // and 9222's ran to ~300s. A missing port in this list says "never repaired again"; a port
        // present in it with `slots=8 listening=8` says "repaired, healthy, and still refusing" --
        // which would put the fault in packet delivery instead, and those need different fixes.
        // `drain_seen` vs `repair_seen` says how many descriptors the two passes reached: the drain
        // takes a shared guard and the repair an exclusive one, so `repair_seen < drain_seen`
        // tick after tick is a descriptor a guest thread is holding across the repair, i.e. a
        // listening port this process can never re-arm.
        {
            static TICKS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            let tick = TICKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if tick % 512 == 0 {
                let drained = alloc::format!("{:?}", drain_ports);
                let reached = alloc::format!("{:?}", reached_ports);
                litebox_util_log::warn!(
                    tick = tick,
                    tag = host_process_tag(),
                    dtag = dtag,
                    drain_seen = drain_seen,
                    repair_seen = repair_seen,
                    drained:% = drained,
                    reached:% = reached;
                    "diag-tick: listening ports this tick's backlog repair actually reached"
                );
            }
        }
    }

    /// Keep a listening port's backlog armed, whatever took a slot away.
    ///
    /// `accept` re-arms on the slots it sees, but it is only called when the port already looks
    /// readable -- so a port whose every slot went stale (a dead-holder `reset_after_poisoning()`
    /// elsewhere wiped them out of the shared socket set, see `socket_set_contains`'s doc comment)
    /// gets NO `accept` call at all, and nothing else in the stack ever re-arms it: every later
    /// SYN is refused for the rest of the session while the connections it already accepted keep
    /// working. Same for a `refill_to_backlog` that found the socket table full: the slot it could
    /// not create is never retried once the table drains. Both were live-measured as one symptom
    /// (chrD92: an in-guest `curl 127.0.0.1:8081` was refused from t=120s to the end of the run
    /// while selkies' accepted websocket went on streaming).
    ///
    /// Only stale slots (no longer in the socket set) are dropped here -- a slot that is still in
    /// the set is never removed from it by this sweep, because the sweep runs from EVERY process's
    /// tick over a socket set shared across the fork family, and dropping a socket another process
    /// allocated runs its ring buffers through the wrong heap (see `remove_dead_sockets`).
    fn repair_listening_backlog(
        listen_owner: &[core::sync::atomic::AtomicU64; LISTEN_OWNER_SLOTS],
        socket_set: &mut smoltcp::iface::SocketSet<'static>,
        buffers: &mut SocketBuffers,
        socket_handle: &mut SocketHandle<Platform>,
        platform: &Platform,
    ) {
        if socket_handle
            .consider_closed
            .load(core::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        // A BORROWED referent of a listening socket (`fork_adopt`'s `"L"` arm) owns no accept queue
        // of its own: it accepts from the queue the OWNING process armed on the endpoint
        // (`Network::unclaimed_connection_on`), which is what makes one listening port shared
        // across `fork()` behave like Linux's shared open file description. Refilling here would
        // undo exactly that -- this sweep runs from every process's tick, so every fork child
        // inheriting the port would arm `backlog` more LISTEN sockets on the same endpoint in the
        // shared socket set, and an incoming SYN would go to whichever queue smoltcp matched
        // first, leaving the owning process's own queue empty while another process's fills with
        // connections nobody accept()s. That is chrF4's competing-queues symptom, measured as
        // selkies' published 8081 answering one request and then refusing every connect for the
        // rest of the run; `fork_adopt` stopped arming them at adoption time but this sweep
        // re-armed them on the child's very next tick. Its slot list therefore stays EMPTY, and
        // its `server_socket` here exists only to name the endpoint and the backlog to accept
        // from -- until the process that owns the queue is GONE, at which point nobody maintains
        // it and the port is deaf for good unless this referent takes it over.
        if socket_handle.borrowed {
            let ProtocolSpecific::Tcp(tcp_specific) = &mut socket_handle.specific else {
                return;
            };
            let Some(server_socket) = tcp_specific.server_socket.as_mut() else {
                return;
            };
            if !Self::promote_borrowed_listener(
                listen_owner,
                socket_set,
                &mut server_socket.socket_set_handles,
                server_socket.ip_listen_endpoint.port,
                platform,
            ) {
                return;
            }
            // Promoted: from here on this referent maintains the queue itself, so refill, reclaim
            // and close all apply to it exactly as they do for the process that created the port.
            socket_handle.borrowed = false;
        }
        let ProtocolSpecific::Tcp(tcp_specific) = &mut socket_handle.specific else {
            return;
        };
        let Some(server_socket) = tcp_specific.server_socket.as_mut() else {
            return;
        };
        let Some(backlog) = server_socket.backlog else {
            return;
        };
        let handles_before = server_socket.socket_set_handles.len();
        server_socket
            .socket_set_handles
            .retain(|&handle| Self::socket_set_contains(socket_set, handle));
        let went_fully_dead =
            server_socket.socket_set_handles.is_empty() && handles_before > 0;
        // A port can hold its full backlog and still be unable to accept a thing: smoltcp
        // dispatches a SYN only to a slot in `Listen`, so a port whose every slot already took a
        // connection the application has not `accept`ed answers every later SYN with an RST --
        // instantly, which is exactly how a "dead port" looks from outside while the connections
        // it already accepted keep working. No slot is stale in that state, so nothing else in the
        // stack ever mentions it; report it once per episode (this sweep runs every tick, from
        // every process of the fork family).
        // A backlog slot no application fd points at can still end up in a state `accept` will
        // never hand out: the peer's FIN lands before the application gets round to `accept`
        // (`CloseWait`, which `accept` does hand out now), or the connection is torn down outright
        // (`Closed`, `TimeWait`, the `FinWait*`/`Closing`/`LastAck` a reset leaves behind). Linux
        // keeps the port listening by reclaiming those slots; so must we, because a slot left in
        // place costs the port one connection of capacity forever and after `backlog` of them the
        // port refuses every SYN for the rest of the session while the connections it already
        // accepted go on streaming (fl6: an in-guest connect answered through fork 7 and was
        // refused from fork 8 on, `backlog` 8, while a control port beside it answered all 40).
        let reclaimed = Self::reclaim_finished_backlog_slots(
            socket_set,
            buffers,
            &mut server_socket.socket_set_handles,
        );
        if reclaimed > 0 {
            static RECLAIMS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            if RECLAIMS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % 64 == 0 {
                litebox_util_log::warn!(
                    port = server_socket.ip_listen_endpoint.port,
                    reclaimed = reclaimed;
                    "diag-listener: reclaimed finished backlog slot(s) so this listening port can re-arm them"
                );
            }
        }
        let mut listening = 0usize;
        let mut pending = 0usize;
        let mut other = 0usize;
        for &handle in &server_socket.socket_set_handles {
            match socket_set.get::<tcp::Socket>(handle).state() {
                tcp::State::Listen => listening += 1,
                // `CloseWait` is a connection whose peer already hung up: `accept` hands it out, so
                // it is pending work for the application, not a slot stuck in limbo.
                tcp::State::Established
                | tcp::State::CloseWait
                | tcp::State::SynReceived
                | tcp::State::SynSent => pending += 1,
                _ => other += 1,
            }
        }
        // `diag-listener` below speaks only when a port goes DEAF, so a port that looks healthy at
        // the exact moment a connect fails leaves no trace at all -- which is why chrF8/chrF9 went
        // unexplained: 8081 streamed to the one client it had accepted while every new connect
        // failed. Say what the port and the shared table look like periodically instead, so the
        // state at any failure can be read straight off the log. Throttled: this sweep runs every
        // tick, from every process of the fork family.
        {
            static HEARTBEATS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            if HEARTBEATS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % 512 == 0 {
                let (data_granted, data_used, meta_granted, meta_used, owners) = buffers.occupancy();
                let (total, census) = socket_census(socket_set);
                litebox_util_log::warn!(
                    tag = host_process_tag(),
                    port = server_socket.ip_listen_endpoint.port,
                    slots = server_socket.socket_set_handles.len(),
                    listening = listening,
                    pending = pending,
                    other = other,
                    sockets = total,
                    max = MAX_SOCKETS,
                    data_granted = data_granted,
                    data_used = data_used,
                    meta_granted = meta_granted,
                    meta_used = meta_used,
                    owners = owners,
                    census:% = census;
                    "diag-port: listening port state and shared socket table occupancy"
                );
            }
        }
        // An emptied `socket_set_handles` counts as "no slot in LISTEN" too: that is what a port
        // looks like once a refill failed (the slots went to accepted connections and the pool had
        // nothing left to replace them), and it is the deadest a listening port can be.
        if listening == 0 {
            if !server_socket.no_slot_listening_reported {
                server_socket.no_slot_listening_reported = true;
                let readable = socket_handle.proxy.as_ref().is_some_and(|proxy| {
                    matches!(proxy.as_ref(), NetworkProxy::Stream(ch) if ch.is_readable())
                });
                litebox_util_log::warn!(
                    port = server_socket.ip_listen_endpoint.port,
                    slots = server_socket.socket_set_handles.len(),
                    pending = pending,
                    other = other,
                    readable = readable;
                    "diag-listener: no backlog slot of this listening port is in LISTEN state; every later SYN is refused until the application accepts"
                );
            }
        } else if server_socket.no_slot_listening_reported {
            server_socket.no_slot_listening_reported = false;
            litebox_util_log::warn!(
                port = server_socket.ip_listen_endpoint.port,
                listening = listening;
                "diag-listener: this listening port has a LISTEN slot again"
            );
        }
        // Nothing to do when the port is armed to its backlog: `accept` handles the slots it
        // reaches, including any that are still in the socket set but no longer open.
        if server_socket.socket_set_handles.len() == handles_before
            && handles_before >= backlog.into()
        {
            return;
        }
        if went_fully_dead {
            litebox_util_log::warn!(
                port = server_socket.ip_listen_endpoint.port;
                "diag-listener: every backlog slot of this listening port went stale, re-arming it"
            );
        }
        // Nothing to re-arm with; the next sweep retries once a slot frees up.
        if socket_set.iter().count() >= MAX_SOCKETS {
            return;
        }
        server_socket.refill_to_backlog(socket_set, buffers);
    }

    /// Drain data between socket channels and smoltcp sockets.
    ///
    /// This transfers data from the TX ring buffer (user writes) to the smoltcp socket,
    /// and from the smoltcp socket to the RX ring buffer (user reads).
    ///
    /// Should be called periodically by the network worker to keep data flowing.
    /// `pull_rx`: this call IS a reader fetching its own bytes, so RX moves into the proxy now
    /// whatever the per-tick rule below would leave in smoltcp.
    fn drain_socket_channel_buffers(
        socket_set: &mut smoltcp::iface::SocketSet<'static>,
        socket_handle: &SocketHandle<Platform>,
        now: smoltcp::time::Instant,
        shared_across_fork: bool,
        pull_rx: bool,
        accepted_slots: &[Option<smoltcp::iface::SocketHandle>; MAX_SOCKETS],
    ) {
        let proxy = match &socket_handle.proxy {
            Some(proxy) => proxy.as_ref(),
            None => return,
        };
        // A socket another process of the fork family also reads: let this process's proxy know,
        // so a read here pulls RX for itself instead of waiting for this tick to deliver it.
        if shared_across_fork {
            match proxy {
                NetworkProxy::Stream(channel) => channel.mark_shared_across_fork(),
                NetworkProxy::Datagram(channel) => channel.mark_shared_across_fork(),
                NetworkProxy::Raw => {}
            }
        }
        // See `Network::socket_set_contains`'s doc comment: a stale handle (this descriptor's
        // own socket, wiped out from under it by a dead-holder `reset_after_poisoning()`
        // elsewhere) has nothing left to drain -- `socket_set.get_mut` would otherwise panic.
        if !socket_set.iter().any(|(h, _)| h == socket_handle.handle) {
            return;
        }
        match (socket_handle.protocol(), proxy) {
            (Protocol::Tcp, NetworkProxy::Stream(proxy)) => {
                let tcp_socket = socket_set.get_mut::<tcp::Socket>(socket_handle.handle);

                while tcp_socket.can_send() {
                    let sent = proxy
                        .pop_tx_data_with(|data| tcp_socket.send_slice(data).unwrap_or_default());
                    if sent == 0 {
                        break;
                    }
                }

                // A local `shutdown(SHUT_WR)` owes the peer a FIN, but only after every byte the
                // application wrote before it has left the TX buffer (sending the FIN straight
                // away dropped the reply of servers that `sendall(); shutdown(WR)`).
                if proxy.is_write_shutdown() && !proxy.has_pending_tx() && tcp_socket.may_send() {
                    tcp_socket.close();
                }

                // NOT done for a socket another process of the fork family also refers to: a proxy
                // is a PER-PROCESS object, so bytes this tick hands to THIS process's proxy are
                // unreachable from the process that IS reading -- its reader waits forever while
                // the bytes sit in a buffer no poll ever looks at. Nor can "is anybody waiting
                // here?" be asked instead: an observer is never unregistered, so a process that
                // has ever blocked on a carried socket keeps counting as "waiting" long after it
                // stopped and goes on eating its child's bytes (measured, `.wfgy/ifrx1a.out`:
                // child `NO TimeoutError('timed out')`, parent `leftover srv=b'PONG'`). Such a
                // socket therefore keeps its RX in smoltcp and the reader fetches it itself
                // ([`Network::drain_rx_into_proxy`]), told it is there by
                // [`StreamSocketChannel::set_smoltcp_rx_pending`].
                if !shared_across_fork || pull_rx {
                    while tcp_socket.can_recv() {
                        let received = proxy
                            .push_rx_data_with(|buf| tcp_socket.recv_slice(buf).unwrap_or_default());
                        if received == 0 {
                            break;
                        }
                    }
                }
                if shared_across_fork {
                    proxy.set_smoltcp_rx_pending(tcp_socket.can_recv());
                }

                if let tcp::State::Established = tcp_socket.state() {
                    proxy.set_state(socket_channel::SocketState::Connected);
                    proxy.clear_async_error();
                }
                let tcp_specific = socket_handle.specific.tcp();
                // The peer's FIN leaves smoltcp's socket "open" (CloseWait/LastAck/Closing) until
                // this side closes too, so the channel never saw the connection end and a reader
                // blocked forever instead of getting EOF (every guest-loopback HTTP response that
                // ends with the server closing hung its client). Report it once the data the peer
                // sent has all been delivered.
                if tcp_specific.server_socket.is_none()
                    && !tcp_socket.can_recv()
                    && matches!(
                        tcp_socket.state(),
                        tcp::State::CloseWait
                            | tcp::State::LastAck
                            | tcp::State::Closing
                            | tcp::State::TimeWait
                    )
                {
                    proxy.mark_peer_closed();
                }
                // server socket that is listening also has closed state
                if !tcp_socket.is_open() && tcp_specific.server_socket.is_none() {
                    match proxy.state() {
                        socket_channel::SocketState::Connecting => {
                            // Socket closed while connecting. Distinguish RST from timeout.
                            let peer_port = tcp_specific.connect_peer_port.unwrap_or(0);
                            // The socket's own endpoints are already gone (that is the whole reason
                            // the peer port is carried on `TcpSpecific`), so take the local port
                            // here too -- and with it the last use of `tcp_socket`, which frees the
                            // socket set to be read again for the slot census below.
                            let local_port = tcp_socket.local_endpoint().map_or(0, |e| e.port);
                            // Taken while `tcp_socket` is still borrowed, because the socket set has
                            // to be readable again for the census below. Two different things end a
                            // connect and only one of them is a refusal: the peer's RST, and this
                            // side closing the socket (a process that exited, `close_handle`, an
                            // abort). Calling both "refused" is what let chrF21 read 68 refusals
                            // without proving a single RST was ever received, so say which one.
                            let state = tcp_socket.state();
                            let closed_here = socket_handle
                                .consider_closed
                                .load(core::sync::atomic::Ordering::Relaxed);
                            let (error, elapsed) = match tcp_specific.connect_initiated_at_us {
                                Some(initiated_at) if now - initiated_at >= TCP_CONNECT_TIMEOUT => {
                                    (errors::SocketAsyncError::TimedOut, now - initiated_at)
                                }
                                Some(initiated_at) => {
                                    (errors::SocketAsyncError::ConnectionRefused, now - initiated_at)
                                }
                                None => (
                                    errors::SocketAsyncError::ConnectionRefused,
                                    smoltcp::time::Duration::ZERO,
                                ),
                            };
                            report_connect_outcome(
                                match error {
                                    errors::SocketAsyncError::TimedOut => "timeout",
                                    errors::SocketAsyncError::ConnectionRefused => "refused",
                                    _ => "other",
                                },
                                peer_port,
                                local_port,
                                elapsed.total_micros(),
                                TCP_CONNECT_TIMEOUT.total_micros(),
                                &port_slots_at(socket_set, peer_port),
                                socket_set.iter().count(),
                                match state {
                                    tcp::State::Listen => "L",
                                    tcp::State::SynReceived => "SR",
                                    tcp::State::SynSent => "SS",
                                    tcp::State::Established => "E",
                                    tcp::State::FinWait1 => "FW1",
                                    tcp::State::FinWait2 => "FW2",
                                    tcp::State::CloseWait => "CW",
                                    tcp::State::Closing => "CG",
                                    tcp::State::LastAck => "LA",
                                    tcp::State::TimeWait => "TW",
                                    tcp::State::Closed => "C",
                                },
                                closed_here,
                            );
                            proxy.set_async_error(error);
                            proxy.set_state(socket_channel::SocketState::Error);
                        }
                        socket_channel::SocketState::Connected => {
                            proxy.set_async_error(errors::SocketAsyncError::ConnectionReset);
                            proxy.set_state(socket_channel::SocketState::Closed);
                        }
                        _ => {
                            proxy.set_state(socket_channel::SocketState::Closed);
                        }
                    }
                }

                if let Some(server_socket) = tcp_specific.server_socket.as_ref()
                    && !proxy.is_readable()
                {
                    let pending = server_socket
                        .socket_set_handles
                        .iter()
                        .any(|&h| {
                            // Same stale-handle guard as above: one of a listening socket's own
                            // accepted-connection handles can independently go stale.
                            socket_set.iter().any(|(live, _)| live == h) && {
                                let socket: &tcp::Socket = socket_set.get(h);
                                // Whatever `accept` hands out must wake the reader, or an
                                // epoll-driven server never learns the connection is there: a peer
                                // that hangs up straight after connecting leaves the slot in
                                // `CloseWait`, which an `Established`-only test misses -- so the
                                // listener is never reported readable, the application never calls
                                // `accept`, and the slot is stranded for the rest of the session.
                                matches!(
                                    socket.state(),
                                    tcp::State::Established | tcp::State::CloseWait
                                )
                            }
                            // A slot another process of the fork family already accepted is not
                            // pending for this one (see `Network::accepted_slots`).
                            && !accepted_slots.iter().any(|marked| *marked == Some(h))
                        })
                        // A fork child's own list is empty by design -- it shares its parent's
                        // queue instead of arming a second listener on the same port -- so queued
                        // connections are looked for on the endpoint itself, or the child would
                        // never be reported readable and never accept a thing.
                        || Self::unclaimed_connection_on(
                            socket_set,
                            accepted_slots,
                            server_socket.ip_listen_endpoint.port,
                        )
                        .is_some();
                    if pending {
                        proxy.set_readable(true);
                        proxy.notify_io_event(Events::IN);
                    }
                }
            }
            (Protocol::Udp, NetworkProxy::Datagram(udp_proxy)) => {
                let udp_socket = socket_set.get_mut::<udp::Socket>(socket_handle.handle);
                let remote_endpoint = socket_handle.udp().remote_endpoint;

                while udp_socket.can_send() {
                    // Try to send - consumes datagram only if closure returns true
                    let result = udp_proxy.try_send_datagram_with(|data, addr| {
                        let destination = addr
                            .map(|s| match s {
                                SocketAddr::V4(addr) => smoltcp::wire::IpEndpoint::from(addr),
                                SocketAddr::V6(_) => unimplemented!(),
                            })
                            .or(remote_endpoint);
                        if let Some(endpoint) = destination {
                            udp_socket.send_slice(data, endpoint).is_ok()
                        } else {
                            // No destination - discard
                            true
                        }
                    });
                    if result != Some(true) {
                        break;
                    }
                }

                // Drain RX: receive from smoltcp, push to channel. Same rule as the TCP arm above,
                // same reason.
                if !shared_across_fork || pull_rx {
                    while udp_socket.can_recv() {
                        let received = udp_proxy.try_recv_datagram_with(|| {
                            let (data, meta) = udp_socket.recv().ok()?;
                            let source_addr = match meta.endpoint.addr {
                                smoltcp::wire::IpAddress::Ipv4(ipv4) => SocketAddr::V4(
                                    core::net::SocketAddrV4::new(ipv4, meta.endpoint.port),
                                ),
                            };
                            Some((data.into(), source_addr))
                        });
                        if received.is_none() {
                            break;
                        }
                    }
                }
                if shared_across_fork {
                    udp_proxy.set_smoltcp_rx_pending(udp_socket.can_recv());
                }
            }
            (Protocol::Icmp | Protocol::Raw { .. }, _) => {
                unimplemented!()
            }
            _ => panic!("Mismatched protocol and proxy type"),
        }
    }

    /// Move `fd`'s socket's RX into this process's proxy right now, whatever the per-tick drain
    /// would have decided.
    ///
    /// The tick leaves a fork-family-shared socket's RX in smoltcp (see
    /// [`Self::drain_socket_channel_buffers`]), so a read -- blocking, non-blocking, or one that
    /// lost the race against another process's tick -- has to fetch for itself rather than report
    /// "no data".
    ///
    /// Returns `false` when `fd` is not a socket this process has a descriptor for.
    pub fn drain_rx_into_proxy(&mut self, fd: &SocketFd<Platform>) -> bool {
        let now = self.now();
        // Best-effort, same reason as `drain_all_socket_channel_buffers`: `net_lock` is held.
        let Some(table) = self.litebox.try_descriptor_table() else {
            return false;
        };
        let Some(entry) = table.get_entry(fd) else {
            return false;
        };
        // The socket's own shared marking still has to travel with the call: it is what tells the
        // drain to clear this proxy's "smoltcp still holds bytes" flag once it has taken them, or
        // the socket would keep reporting readable with nothing left to read.
        let shared_across_fork = self.is_shared_across_fork(entry.entry.handle);
        Self::drain_socket_channel_buffers(
            &mut self.socket_set,
            &entry.entry,
            now,
            shared_across_fork,
            true,
            &self.accepted_slots,
        );
        true
    }
}

impl<Platform> Network<Platform>
where
    Platform: platform::IPInterfaceProvider
        + platform::TimeProvider
        + sync::RawSyncPrimitivesProvider
        + platform::SharedKernelStateProvider
        + platform::SystemInfoProvider,
{
    fn now(&self) -> smoltcp::time::Instant {
        smoltcp::time::Instant::from_micros(
            // This conversion from u128 to i64 should practically never fail, since 2^63
            // microseconds is roughly 250 years. If a system has been up for that long, then it
            // deserves to panic.
            i64::try_from(
                self.device
                    .platform
                    .now()
                    .duration_since(&self.zero_time)
                    .as_micros(),
            )
            .unwrap(),
        )
    }

    /// A `socket(2)` the guest asked for and could not have: the socket table is full, or one of
    /// the shared buffer pools has no slot left. This is `EMFILE` to the guest, and before this
    /// line it was INVISIBLE at every log level -- which is why chrF8/chrF9 looked like a dead
    /// port: selkies' 8081 went on streaming to the client it had already accepted while every new
    /// connection failed right here. `diag-listener` stayed quiet through it because the port still
    /// had its LISTEN slots, and `diag-pool` because no port was refilling a backlog. Throttled:
    /// this is a guest-reachable failure path, so it can fire very often under load.
    fn warn_socket_refused(&self, protocol: &'static str, reason: &'static str) {
        static REFUSED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        if REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % 64 != 0 {
            return;
        }
        let (data_granted, data_used, meta_granted, meta_used, owners) = self.buffers.occupancy();
        let (total, census) = socket_census(&self.socket_set);
        litebox_util_log::warn!(
            protocol:% = protocol,
            reason:% = reason,
            sockets = total,
            max = MAX_SOCKETS,
            data_granted = data_granted,
            data_used = data_used,
            meta_granted = meta_granted,
            meta_used = meta_used,
            owners = owners,
            census:% = census;
            "diag-socket: socket(2) refused, the socket table or a shared buffer pool has no slot left"
        );
    }

    /// Creates a socket.
    ///
    /// By default, the created socket has no associated proxy; to set a proxy, use
    /// [`set_socket_proxy`](Self::set_socket_proxy).
    pub fn socket(&mut self, protocol: Protocol) -> Result<SocketFd<Platform>, SocketError> {
        // `socket_set` is now a fixed-capacity (`MAX_SOCKETS`) arena-backed slice (see
        // `MAX_SOCKETS`'s own doc comment) -- `smoltcp::iface::SocketSet::add` PANICS on a full
        // fixed-capacity table rather than growing, unlike the old `Vec`-backed one, so check
        // capacity ourselves first and return an ordinary error instead.
        if self.socket_set.iter().count() >= MAX_SOCKETS {
            self.warn_socket_refused("any", "socket table full");
            return Err(SocketError::TooManySockets);
        }
        let handle = match protocol {
            Protocol::Tcp => {
                let Some((rx, tx, claim)) = self.buffers.tcp() else {
                    self.warn_socket_refused("tcp", "no buffer slot left");
                    return Err(SocketError::TooManySockets);
                };
                let handle = self.socket_set.add(tcp::Socket::new(rx, tx));
                self.buffers.adopt(handle, claim);
                handle
            }
            Protocol::Udp => {
                let Some((rx, tx, claim)) = self.buffers.udp() else {
                    self.warn_socket_refused("udp", "no buffer slot left");
                    return Err(SocketError::TooManySockets);
                };
                let handle = self.socket_set.add(udp::Socket::new(rx, tx));
                self.buffers.adopt(handle, claim);
                handle
            }
            // ICMP sockets were created and then hit `unimplemented!()` below; refuse them up
            // front instead of panicking a guest-reachable path.
            Protocol::Icmp => return Err(SocketError::UnsupportedProtocol(1)),
            Protocol::Raw { protocol } => {
                // TODO: Should we maintain a specific allow-list of protocols for raw sockets?
                // Should we allow everything except TCP/UDP/ICMP? Should we allow everything? These
                // questions should be resolved; for now I am disallowing everything else.
                return Err(SocketError::UnsupportedProtocol(protocol));

                #[expect(
                    unreachable_code,
                    reason = "currently raw is just directly disallowed; we might bring this code back in the future"
                )]
                self.socket_set.add(raw::Socket::new(
                    smoltcp::wire::IpVersion::Ipv4,
                    smoltcp::wire::IpProtocol::from(protocol),
                    smoltcp::storage::PacketBuffer::new(
                        vec![smoltcp::storage::PacketMetadata::EMPTY; MAX_PACKET_COUNT],
                        vec![0u8; SOCKET_BUFFER_SIZE],
                    ),
                    smoltcp::storage::PacketBuffer::new(
                        vec![smoltcp::storage::PacketMetadata::EMPTY; MAX_PACKET_COUNT],
                        vec![0u8; SOCKET_BUFFER_SIZE],
                    ),
                ))
            }
        };

        Ok(self.new_socket_fd_for(SocketHandle {
            consider_closed: core::sync::atomic::AtomicBool::new(false),
            shutdown_wr_pending: false,
            handle,
            specific: match protocol {
                Protocol::Tcp => ProtocolSpecific::Tcp(TcpSpecific {
                    local_port: None,
                    server_socket: None,
                    immediate_close: AtomicBool::new(false),
                    connect_initiated_at_us: None,
                    connect_peer_port: None,
                }),
                Protocol::Udp => ProtocolSpecific::Udp(UdpSpecific {
                    remote_endpoint: None,
                }),
                Protocol::Icmp => unimplemented!(),
                Protocol::Raw { protocol: _ } => unimplemented!(),
            },
            proxy: None,
            borrowed: false,
            own_slot: false,
        }))
    }

    fn new_socket_fd_for(&mut self, socket_handle: SocketHandle<Platform>) -> SocketFd<Platform> {
        self.litebox.descriptor_table_mut().insert(socket_handle)
    }

    /// Set the network proxy for the socket at `fd`
    ///
    /// Associating a proxy enables event notification and sending/receiving data without accessing
    /// [`Network`] (which may help avoid lock contention but still requires a periodic call to
    /// [`perform_platform_interaction`](Self::perform_platform_interaction) to move data between smoltcp
    /// and the socket channels though).
    ///
    /// If no proxy is set, then the socket can still be used for sending/receiving data via [`Network`]
    /// interfaces like [`send`](Self::send)/[`receive`](Self::receive), but no events will be notified.
    #[must_use]
    pub fn set_socket_proxy(
        &mut self,
        fd: &SocketFd<Platform>,
        proxy: alloc::sync::Arc<NetworkProxy<Platform>>,
    ) -> bool {
        let descriptor_table = self.litebox.descriptor_table();
        let Some(mut table_entry) = descriptor_table.get_entry_mut(fd) else {
            return false;
        };
        // The proxy this process reads from has to know when the socket is shared with another
        // process of the fork family, because the tick leaves such a socket's RX in smoltcp and a
        // reader has to pull it for itself.
        let shared_across_fork = {
            let borrowed = table_entry.entry.borrowed;
            let handle = table_entry.entry.handle;
            borrowed || self.shared_across_fork.iter().any(|marked| *marked == Some(handle))
        };
        if shared_across_fork {
            match proxy.as_ref() {
                NetworkProxy::Stream(channel) => channel.mark_shared_across_fork(),
                NetworkProxy::Datagram(channel) => channel.mark_shared_across_fork(),
                NetworkProxy::Raw => {}
            }
        }
        let socket_handle = &mut table_entry.entry;
        socket_handle.proxy = Some(proxy);
        true
    }

    /// Describes the socket at `fd` for a cross-process `fork()` child: the minimum a child that
    /// is a DIFFERENT host process needs to attach a second reference to this very socket, as an
    /// ASCII spec. `None` when this socket's state cannot be named that way, in which case the
    /// caller drops the fd in the child rather than refusing the fork (see
    /// `litebox_shim_linux::syscalls::process::try_cross_process_fork`).
    ///
    /// Nothing here carries a pointer or a private-heap allocation, because the spec crosses a
    /// `CreateProcessW` boundary in the child's environment. It names the socket by its ENDPOINTS
    /// rather than by its `smoltcp` slot index: `SocketHandle`'s index is private to smoltcp and
    /// the slot ordering it would encode is not stable across the ~1s the child takes to boot
    /// (a sibling socket created or closed in between shifts every later ordinal).
    ///
    /// What actually makes the carry work is that `socket_set`'s storage, every socket's rx/tx
    /// buffers and the local-port refcount table are all shared-kernel-arena-native: the child
    /// attaches to the SAME `Network`, so a socket it finds there is the parent's socket.
    ///
    /// Shapes: `T,<lip>,<lport>,<rip>,<rport>` (TCP, connected), `L,<lip>,<lport>,<backlog>`
    /// (TCP, listening), `U,<lip>,<lport>,<rip>,<rport>` (UDP, bound), `u` (UDP, never bound).
    /// All numbers hex. `<rip>/<rport>` are `0` when there is no connected peer.
    pub fn fork_carry_spec(&self, fd: &SocketFd<Platform>) -> Option<alloc::string::String> {
        let descriptor_table = self.litebox.descriptor_table();
        let table_entry = descriptor_table.get_entry_mut(fd)?;
        let socket_handle = &table_entry.entry;
        if !Self::socket_set_contains(&self.socket_set, socket_handle.handle) {
            return None;
        }
        match socket_handle.protocol() {
            Protocol::Tcp => {
                if let Some(server) = socket_handle.tcp().server_socket.as_ref() {
                    let endpoint = server.ip_listen_endpoint;
                    let backlog = server.backlog?;
                    return Some(alloc::format!(
                        "L,{:x},{:x},{:x}",
                        v4_to_u32(endpoint.addr),
                        endpoint.port,
                        backlog
                    ));
                }
                let socket: &tcp::Socket = self.socket_set.get(socket_handle.handle);
                // A bound-but-unconnected, non-listening TCP socket has no endpoint smoltcp knows,
                // so there is nothing a child could match it on -- and nothing to share yet either.
                let (local, remote) = (socket.local_endpoint()?, socket.remote_endpoint()?);
                Some(alloc::format!(
                    "T,{:x},{:x},{:x},{:x}",
                    v4_to_u32(Some(local.addr)),
                    local.port,
                    v4_to_u32(Some(remote.addr)),
                    remote.port
                ))
            }
            Protocol::Udp => {
                let endpoint = self.socket_set.get::<udp::Socket>(socket_handle.handle).endpoint();
                let remote = socket_handle.udp().remote_endpoint;
                if endpoint.port == 0 && remote.is_none() {
                    // Never bound, never connected: the child can simply make its own.
                    return Some(alloc::string::String::from("u"));
                }
                Some(alloc::format!(
                    "U,{:x},{:x},{:x},{:x}",
                    v4_to_u32(endpoint.addr),
                    endpoint.port,
                    v4_to_u32(remote.map(|ep| ep.addr)),
                    remote.map_or(0, |ep| ep.port)
                ))
            }
            Protocol::Icmp | Protocol::Raw { .. } => None,
        }
    }

    /// Rebuilds, in a cross-process `fork()` child, the socket [`Self::fork_carry_spec`]
    /// described: a second reference in THIS process's descriptor table to the same socket.
    ///
    /// Returns `None` (never panics) when the socket cannot be found or recreated -- most often
    /// because the parent closed it, or the socket table/buffer pool is exhausted -- and the
    /// caller then leaves the guest fd missing, which the guest sees as `EBADF`.
    pub fn fork_adopt(&mut self, spec: &str) -> Option<SocketFd<Platform>> {
        let mut parts = spec.split(',');
        let kind = parts.next()?;
        let hex = |p: Option<&str>| -> Option<u32> { u32::from_str_radix(p?, 16).ok() };
        match kind {
            // TCP, connected: find the one socket whose local AND remote endpoints match. Both
            // are needed: a client's local port alone is not unique across two connections to
            // different peers, and the pair is.
            "T" => {
                let (lip, lport, rip, rport) = (
                    hex(parts.next())?,
                    hex(parts.next())? as u16,
                    hex(parts.next())?,
                    hex(parts.next())? as u16,
                );
                let local = smoltcp::wire::IpEndpoint {
                    addr: smoltcp::wire::IpAddress::Ipv4(u32_to_v4(lip)),
                    port: lport,
                };
                let remote = smoltcp::wire::IpEndpoint {
                    addr: smoltcp::wire::IpAddress::Ipv4(u32_to_v4(rip)),
                    port: rport,
                };
                let handle = self.socket_set.iter().find_map(|(handle, socket)| {
                    let smoltcp::socket::Socket::Tcp(socket) = socket else {
                        return None;
                    };
                    (socket.local_endpoint() == Some(local) && socket.remote_endpoint() == Some(remote))
                        .then_some(handle)
                })?;
                // From here on this socket has TWO referents, in two processes, and only the one
                // that reads it may take its RX (see `drain_socket_channel_buffers`).
                self.mark_shared_across_fork(handle);
                Some(self.new_socket_fd_for(SocketHandle {
                    consider_closed: core::sync::atomic::AtomicBool::new(false),
                    shutdown_wr_pending: false,
                    handle,
                    specific: ProtocolSpecific::Tcp(TcpSpecific {
                        // Deliberately `None`: the parent owns the port's refcount, and a token
                        // here would make this child's `close()` free a port the parent still
                        // binds. `getsockname` does not need it -- a connected socket's address
                        // comes from smoltcp's own `local_endpoint()`.
                        local_port: None,
                        server_socket: None,
                        immediate_close: AtomicBool::new(false),
                        connect_initiated_at_us: None,
                        connect_peer_port: None,
                    }),
                    proxy: None,
                    // Adopted: `handle` is the parent's socket, still in use by the parent.
                    borrowed: true,
                    own_slot: false,
                }))
            }
            // TCP, listening: the child gets a SECOND REFERENT of the listening socket, not a
            // second listener. Arming its own backlog slots here used to give the port two
            // independent accept queues -- the parent's and the child's -- both `listen()`ing on
            // the same endpoint in the shared smoltcp set, so an incoming SYN went to whichever
            // smoltcp matched first and the other queue sat empty: a child that inherits a
            // listening socket and is the one that runs the accept loop (exactly what a server
            // that forks per client does) accepted nothing, while connections already queued on
            // the parent went unserved until the parent got round to them. Measured as selkies'
            // published 8081 answering one request and then refusing every connect in 20ms for
            // the rest of a 900s run (chrF4), against the same recipe holding code=200 throughout
            // before CLOEXEC sockets crossed a fork (chrF3). Linux shares the open file
            // description, so the child accepts from the queue the parent owns: no slots are
            // armed here, and `accept`/the readiness sweep look for queued connections on the
            // endpoint itself (`Network::unclaimed_connection_on`).
            "L" => {
                let (lip, lport, backlog) = (
                    hex(parts.next())?,
                    hex(parts.next())? as u16,
                    hex(parts.next())? as u16,
                );
                if self.socket_set.iter().count() >= MAX_SOCKETS {
                    return None;
                }
                let (rx, tx, claim) = self.buffers.tcp()?;
                let handle = self.socket_set.add(tcp::Socket::new(rx, tx));
                self.buffers.adopt(handle, claim);
                // How often a fork child inherits a listening port, and what the shared socket set
                // looks like when it does: each adoption costs one socket slot for the life of the
                // child, and 8081's refusal in chrF10/chrF11 wanted to know whether these pile up.
                {
                    static ADOPTS: core::sync::atomic::AtomicU32 =
                        core::sync::atomic::AtomicU32::new(0);
                    if ADOPTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % 64 == 0 {
                        litebox_util_log::warn!(
                            port = lport,
                            sockets = self.socket_set.iter().count(),
                            max = MAX_SOCKETS;
                            "diag-adopt: a fork child inherited a listening port (borrowed, no slots of its own)"
                        );
                    }
                }
                Some(self.new_socket_fd_for(SocketHandle {
                    consider_closed: core::sync::atomic::AtomicBool::new(false),
                    shutdown_wr_pending: false,
                    handle,
                    specific: ProtocolSpecific::Tcp(TcpSpecific {
                        local_port: None,
                        server_socket: Some(TcpServerSpecific {
                            ip_listen_endpoint: smoltcp::wire::IpListenEndpoint {
                                addr: Some(smoltcp::wire::IpAddress::Ipv4(u32_to_v4(lip))),
                                port: lport,
                            },
                            backlog: Some(backlog.max(1)),
                            socket_set_handles: Vec::new(),
                            no_slot_listening_reported: false,
                        }),
                        immediate_close: AtomicBool::new(false),
                        connect_initiated_at_us: None,
                        connect_peer_port: None,
                    }),
                    proxy: None,
                    // The port's listening slots belong to the parent: this process's own close
                    // must not tear the queue down, same as every other borrowed arm.
                    borrowed: true,
                    // ...but the socket itself was added above, for this referent alone.
                    own_slot: true,
                }))
            }
            // UDP, bound: found by its bound endpoint (unique per the local-port allocator).
            "U" => {
                let (lip, lport, rip, rport) = (
                    hex(parts.next())?,
                    hex(parts.next())? as u16,
                    hex(parts.next())?,
                    hex(parts.next())? as u16,
                );
                let handle = self.socket_set.iter().find_map(|(handle, socket)| {
                    let smoltcp::socket::Socket::Udp(socket) = socket else {
                        return None;
                    };
                    let endpoint = socket.endpoint();
                    (endpoint.port == lport && v4_to_u32(endpoint.addr) == lip).then_some(handle)
                })?;
                // Two referents in two processes from here on: see the TCP arm above.
                self.mark_shared_across_fork(handle);
                Some(self.new_socket_fd_for(SocketHandle {
                    consider_closed: core::sync::atomic::AtomicBool::new(false),
                    shutdown_wr_pending: false,
                    handle,
                    specific: ProtocolSpecific::Udp(UdpSpecific {
                        remote_endpoint: (rport != 0).then(|| smoltcp::wire::IpEndpoint {
                            addr: smoltcp::wire::IpAddress::Ipv4(u32_to_v4(rip)),
                            port: rport,
                        }),
                    }),
                    proxy: None,
                    // Adopted: `handle` is the parent's UDP socket, still in use by the parent.
                    borrowed: true,
                    own_slot: false,
                }))
            }
            // UDP, never bound: nothing to share, so the child gets its own fresh socket.
            "u" => {
                if self.socket_set.iter().count() >= MAX_SOCKETS {
                    return None;
                }
                let (rx, tx, claim) = self.buffers.udp()?;
                let handle = self.socket_set.add(udp::Socket::new(rx, tx));
                self.buffers.adopt(handle, claim);
                Some(self.new_socket_fd_for(SocketHandle {
                    consider_closed: core::sync::atomic::AtomicBool::new(false),
                    shutdown_wr_pending: false,
                    handle,
                    specific: ProtocolSpecific::Udp(UdpSpecific {
                        remote_endpoint: None,
                    }),
                    proxy: None,
                    borrowed: false,
                    // A socket of its own, but one `closing_in_background` retires, not this arm.
                    own_slot: false,
                }))
            }
            _ => None,
        }
    }

    pub fn close(
        &mut self,
        fd: &SocketFd<Platform>,
        behavior: CloseBehavior,
    ) -> Result<(), CloseError> {
        // Taken before the descriptor table is borrowed: the closure below needs
        // `&mut self.socket_set`, so it cannot also borrow all of `self` to ask for the time.
        let now = self.now();
        let mut dt = self.litebox.descriptor_table_mut();
        match dt
            .close_and_duplicate_if_shared(fd, |entry| {
                match behavior {
                    CloseBehavior::Immediate => {
                        let socket_handle = &entry.entry;
                        if let crate::net::Protocol::Tcp = socket_handle.protocol() {
                            socket_handle
                                .tcp()
                                .immediate_close
                                .store(true, Ordering::SeqCst);
                        }
                        return true;
                    }
                    // Falls through to the pending-data check below, exactly like
                    // `GracefulIfNoPendingData`. The two differ only in what the CALLER is told
                    // when data is still queued (see the `Deferred` arm), never in whether that
                    // data is allowed to be thrown away.
                    //
                    // This used to `return true`, closing the socket at once and discarding
                    // anything the guest had written but that had not yet reached smoltcp. That is
                    // not what `close(2)` does: with default linger settings Linux flushes queued
                    // data in the background and sends FIN afterwards. And it is the DEFAULT path
                    // -- `SO_LINGER` unset maps here -- so the ordinary
                    // `write(fd, ...); close(fd);` a server does at the end of a response lost the
                    // response whenever the drain had not run in between.
                    //
                    // Measured on a `--publish`ed port: the guest logged `GUEST_SENT 76`, the host
                    // got nothing, and the only packets the guest ever transmitted were SYN-ACK,
                    // FIN and ACK -- no payload at all. It reproduced as a RACE, working whenever
                    // logging slowed the run enough for the drain to happen first, which is what a
                    // discarded buffer looks like from the outside.
                    CloseBehavior::Graceful | CloseBehavior::GracefulIfNoPendingData => {
                        // Hand the guest's queued bytes to smoltcp BEFORE deciding whether this
                        // socket has to stay open. The TX ring is a plain `HeapRb` in THIS
                        // process's heap (`socket_channel.rs`) and the descriptor lives in this
                        // process's table, so once this process is gone root's
                        // `drain_all_socket_channel_buffers` -- which walks root's OWN descriptor
                        // table -- can never reach those bytes. Measured: a guest that wrote a
                        // 43-byte response, closed and exited at once delivered NOTHING at all,
                        // while the same write with a 0.3s pause before exiting delivered it
                        // (`sock9`/`sock10`). After this call smoltcp owns the bytes, and smoltcp
                        // lives in the shared socket set, so root transmits them -- and the FIN
                        // `close_handle` queues -- even if this process dies the instant
                        // `close()` returns.
                        let handle = entry.entry.handle;
                        let shared_across_fork = self
                            .shared_across_fork
                            .iter()
                            .any(|marked| *marked == Some(handle));
                        Self::drain_socket_channel_buffers(
                            &mut self.socket_set,
                            &entry.entry,
                            now,
                            shared_across_fork,
                            false,
                            &self.accepted_slots,
                        );
                    }
                }
                // check if there is pending data to be sent
                let socket_handle = &entry.entry;
                if let Some(proxy) = &socket_handle.proxy
                    && proxy.has_pending_tx()
                {
                    return false;
                }
                // Same stale-handle guard as `remove_dead_sockets`/`close_pending_sockets`
                // (`socket_set_contains`'s own doc comment): a dead-holder
                // `reset_after_poisoning()` elsewhere may have already wiped this FD's socket
                // out of `socket_set` before this guest `close()` call ever reached it -- this
                // was the one remaining guest-reachable call site still going straight to
                // `with_socket`'s unguarded `SocketSet::get`, live-caught as a real
                // `"handle does not refer to a valid socket"` smoltcp panic that killed a whole
                // cross-process-fork child's guest-execution thread outright (AGENTS.md,
                // twenty-eighth pass). Nothing is left to flush for a socket that's already
                // gone, so this closes immediately, matching a real Linux double-close's
                // benign-no-op spirit, instead of panicking on a guest-reachable path.
                if !Self::socket_set_contains(&self.socket_set, socket_handle.handle) {
                    return true;
                }
                let queued = socket_handle.with_socket(
                    &self.socket_set,
                    |tcp_socket| tcp_socket.may_send() && tcp_socket.send_queue() > 0,
                    |udp_socket| udp_socket.is_open() && udp_socket.send_queue() > 0,
                );
                if matches!(behavior, CloseBehavior::Graceful) {
                    // `close(2)` with linger unset: Linux takes the queued bytes, sends FIN after
                    // them, and returns immediately -- it does not wait. Deferring here instead
                    // leaves the FIN to a `close_pending_sockets` tick in THIS process, which a
                    // process that exits right after `close()` never gets, so the peer saw neither
                    // the payload nor a FIN. `close_handle` below queues this socket's FIN on
                    // descriptor-independent state, so root finishes the exchange either way.
                    return true;
                }
                !queued
            })
            .ok_or(CloseError::InvalidFd)?
        {
            super::fd::CloseResult::Closed(socket_handle) => {
                drop(dt);
                self.close_handle(socket_handle.entry);
            }
            super::fd::CloseResult::Duplicated(dup_fd) => {
                if let Some(slot) = self.queued_for_closure.iter_mut().find(|s| s.is_none()) {
                    *slot = Some(dup_fd);
                }
                // Else: `MAX_SOCKETS` slots (matching every other live-socket-capacity bound in
                // this struct) are already all occupied by other pending-closure duplicates --
                // dropping `dup_fd` here on a guest-reachable path is a disclosed, deliberate
                // trade-off (this specific duplicate's underlying entry stays open a little
                // longer than ideal) rather than adding a NEW `.expect()`/panic to a class this
                // whole struct already went to real effort to remove (twenty-eighth pass).
            }
            super::fd::CloseResult::Deferred => {
                let Some(()) = dt.with_entry(fd, |entry| {
                    entry
                        .entry
                        .consider_closed
                        .store(true, core::sync::atomic::Ordering::Relaxed);
                }) else {
                    // The entry vanished between the defer decision and this store -- a concurrent
                    // close of the same number won the race. There is nothing left to defer and
                    // nothing left to flush, so `close(2)` has succeeded as far as the guest can
                    // tell; killing the whole session here is the one answer Linux never gives.
                    litebox_util_log::warn!(
                        "diag-sock-close: deferred close found no descriptor entry to mark, treating as closed"
                    );
                    return Ok(());
                };
                // `close_pending_sockets` now owns this socket: it closes once the TX ring and
                // smoltcp's send queue have both drained.
                //
                // Whether that is an ERROR depends on what the caller asked for, not on what
                // happened. `GracefulIfNoPendingData` is `SO_LINGER` with a timeout -- the caller
                // wants to know there is still data so it can wait -- while a plain `Graceful`
                // close is `close(2)` with linger unset, which succeeds immediately and leaves
                // the kernel to finish sending. Reporting `DataPending` for the latter would make
                // an ordinary successful close look like a failure.
                return match behavior {
                    CloseBehavior::GracefulIfNoPendingData => Err(CloseError::DataPending),
                    CloseBehavior::Graceful | CloseBehavior::Immediate => Ok(()),
                };
            }
        }
        Ok(())
    }

    /// Attempt to close as many queued-to-close FDs as possible. Returns `true` iff any of them
    /// were closed.
    fn attempt_to_close_queued(&mut self) -> bool {
        if self.queued_for_closure.iter().all(Option::is_none) {
            return false;
        }
        // Never park in here. This runs on the net worker with `net_lock` -- an arena-resident,
        // cross-process mutex -- already held, and with the descriptor table's write lock taken
        // below; a guest thread that holds one descriptor's entry lock and then asks for either of
        // those is waiting for US, so waiting for that entry closes the cycle and parks every other
        // process in the fork family behind `net_lock` (live: 16 host processes queued on one arena
        // mutex with the holder alive, chrD5). Both acquisitions are therefore best-effort: an fd
        // we cannot lock right now stays queued and is retried on the next pass, which is exactly
        // the trade-off `Descriptors::iter_mut_nowait` already documents for this same worker.
        let Some(mut dt) = self.litebox.try_descriptor_table_mut() else {
            return false;
        };
        let entries = dt.drain_entries_full_covered_by_nowait(&mut self.queued_for_closure);
        drop(dt);
        if entries.is_empty() {
            return false;
        }
        for entry in entries {
            self.close_handle(entry.entry);
        }
        true
    }

    /// `pub` so the shim can retire a socket it holds outside the descriptor table (see
    /// `AnyDupFd::release_undelivered_duplicate`): the entry is only handed back when nothing else
    /// refers to it, and that object still has to be closed, never dropped.
    /// `pub(crate)`: `SocketHandle` itself is crate-private, so this stays inside the crate and the
    /// shim reaches it through [`Self::release_duplicate_descriptor`].
    /// Releases a DUPLICATE descriptor (`Descriptors::duplicate`) that was made for a donation
    /// which is in fact crossing a process boundary: the cross-process AF_UNIX data plane carries a
    /// donated descriptor as a text spec, rebuilt on the receiving side, and never hands the
    /// `TypedFd` to anybody -- so the duplicate has no receiver and would sit in the sender's
    /// descriptor table for the rest of the session, holding an `Arc` reference to the very entry
    /// the sender's own later `close(2)` has to close. With it there, that `close()` sees a shared
    /// entry and reports `CloseResult::Duplicated`, which never runs the subsystem close: a
    /// listening socket donated to a fork child and then closed by the parent, with the child
    /// reaped, kept answering its port, where a listener closed without a donation is
    /// `ECONNREFUSED` (xproc29).
    ///
    /// This drops exactly `fd`'s own reference: the object stays alive as long as any other
    /// descriptor names it. Only when none does -- the sender's own fd was closed concurrently -- is
    /// the socket closed properly here rather than dropped.
    pub fn release_duplicate_descriptor(&mut self, fd: &SocketFd<Platform>) {
        let last_reference = self.litebox.descriptor_table_mut().remove(fd);
        if let Some(descriptor_entry) = last_reference {
            self.close_handle(descriptor_entry.entry);
        }
    }

    pub(crate) fn close_handle(&mut self, socket_handle: SocketHandle<Platform>) {
        let SocketHandle {
            consider_closed: _,
            shutdown_wr_pending: _,
            handle,
            mut specific,
            proxy,
            borrowed,
            own_slot,
        } = socket_handle;
        // A BORROWED reference (a cross-process `fork()` carry, see `Network::fork_adopt`) is only
        // this process's own handle on a socket some other process in the fork family owns, so
        // releasing it must release exactly that and nothing else: no smoltcp `close()`/`abort()`
        // on the shared socket, no `LocalPort` deallocation, no `closing_in_background` entry --
        // any of those would tear down a connection the owning process is still using, which is
        // precisely what real Linux's per-`fork()` file-descriptor refcount prevents. The one
        // thing this process DID create for itself is a carried TCP listener's OWN handle
        // (`fork_adopt`'s `"L"` arm adds one socket to name the endpoint), and that one is left
        // alone: it is not in `socket_set`'s closing path either way. A borrowed listener arms no
        // backlog slots of its own (`repair_listening_backlog` returns early for it), so the loop
        // below finds an empty list and removes nothing -- kept because a handle in that list would
        // have to be released here if one ever existed. Without this branch, an inherited TCP connection died the
        // moment the child that inherited it exited -- the parent's own fd survived but pointed
        // at an aborted socket.
        if borrowed {
            if let ProtocolSpecific::Tcp(tcp_specific) = &mut specific
                && let Some(server_socket) = tcp_specific.server_socket.take()
            {
                for handle in server_socket.socket_set_handles {
                    if Self::socket_set_contains(&self.socket_set, handle) {
                        let _ =
                            Self::remove_socket(&mut self.socket_set, &mut self.buffers, handle);
                    }
                }
            }
            if let Some(proxy) = proxy {
                proxy.set_state(socket_channel::SocketState::Closed);
            }
            if !own_slot {
                return;
            }
            // A socket this process added to the shared set just to name an endpoint it inherited
            // (`own_slot`): no other process ever had a referent to it, so releasing this reference
            // is what retires it. It is not the borrowed connection/listener it names -- that one
            // belongs to another process and is deliberately left alone above. Without this, every
            // adoption of a listening port cost a socket-table slot and two shared buffer claims
            // for the rest of the session, which a long desktop session cannot afford
            // (`MAX_SOCKETS` 256, `MAX_DATA_SLOTS` 512 -- see `report_exhausted_buffer_pool`).
            if Self::socket_set_contains(&self.socket_set, handle) {
                // Dropped, not `mem::forget` like `remove_dead_sockets`: that one may be retiring a
                // socket ANOTHER process created (whose `PacketBuffer` vecs live on that process's
                // private heap), while this one was added by this very process.
                let _ = Self::remove_socket(&mut self.socket_set, &mut self.buffers, handle);
            }
            return;
        }
        // Stale-handle guard at each `socket_set` touch point below (see `socket_set_contains`'s
        // own doc comment): a dead-holder `reset_after_poisoning()` elsewhere may already have
        // wiped `handle` (and/or a TCP listening socket's own backlog handles) out of
        // `socket_set` before this call ever reached it -- `SocketSet::remove`/`get_mut` both
        // panic on a handle they no longer have (live-caught: this exact call chain killed a
        // whole cross-process-fork child's guest-execution thread outright, AGENTS.md
        // twenty-eighth pass -- the `close()` call site that reaches here was fixed first, but
        // this function has its own, deeper, independent set of the same unguarded accesses).
        // The rest of this function's bookkeeping (port deallocation, `closing_in_background`,
        // proxy state) is independent of `socket_set` and still runs exactly as before either
        // way -- only the smoltcp-side close/abort is skipped when there is nothing left to
        // close or abort.
        let handle_is_live = Self::socket_set_contains(&self.socket_set, handle);
        match specific.protocol() {
            Protocol::Raw { .. } | Protocol::Icmp => {
                // There is no close/abort for raw and icmp sockets
                if handle_is_live {
                    let _ = Self::remove_socket(&mut self.socket_set, &mut self.buffers, handle);
                }
            }
            Protocol::Udp => {
                if handle_is_live {
                    let smoltcp::socket::Socket::Udp(mut socket) = Self::remove_socket(&mut self.socket_set, &mut self.buffers, handle)
                    else {
                        unreachable!()
                    };
                    self.local_port_allocator
                        .deallocate_port(socket.endpoint().port);
                    socket.close();
                }
            }
            Protocol::Tcp => {
                let tcp_specific = specific.tcp_mut();
                if let Some(server_socket) = tcp_specific.server_socket.take() {
                    for handle in server_socket.socket_set_handles {
                        if Self::socket_set_contains(&self.socket_set, handle) {
                            let _ = Self::remove_socket(&mut self.socket_set, &mut self.buffers, handle);
                        }
                    }
                }
                if let Some(local_port) = tcp_specific.local_port.take() {
                    self.local_port_allocator.deallocate(local_port);
                }
                if handle_is_live {
                    let tcp_socket: &mut tcp::Socket = self.socket_set.get_mut(handle);
                    if tcp_specific.immediate_close.load(Ordering::Relaxed) {
                        tcp_socket.abort();
                    } else {
                        tcp_socket.close();
                    }
                    let slot = self
                        .closing_in_background
                        .iter_mut()
                        .find(|slot| slot.is_none())
                        .expect(
                            "closing_in_background has MAX_SOCKETS slots, one per possible live \
                             socket_set entry -- a socket being closed here always currently \
                             occupies one, so a free slot always exists",
                        );
                    *slot = Some(handle);
                }
            }
        }
        if let Some(proxy) = proxy {
            proxy.set_state(socket_channel::SocketState::Closed);
        }
        self.automated_platform_interaction(PollDirection::Both);
    }

    /// Initiate a connection to an IP address
    ///
    /// When `check_progress` is false, this function attempts to initiate a connection.
    /// Otherwise, this function checks the progress of an ongoing connection.
    pub fn connect(
        &mut self,
        fd: &SocketFd<Platform>,
        addr: &SocketAddr,
        check_progress: bool,
    ) -> Result<(), ConnectError> {
        let SocketAddr::V4(addr) = addr else {
            return Err(ConnectError::UnsupportedAddress(*addr));
        };

        let descriptor_table = self.litebox.descriptor_table();
        let mut table_entry = descriptor_table
            .get_entry_mut(fd)
            .ok_or(ConnectError::InvalidFd)?;
        let socket_handle = &mut table_entry.entry;
        let now = self.now();
        let ret = match socket_handle.protocol() {
            Protocol::Tcp => {
                let check_state = |state: tcp::State| -> Result<(), ConnectError> {
                    match state {
                        tcp::State::Established => {
                            Ok(())
                        }
                        // The handshake completed and the peer is already shutting the connection
                        // down (or this side is): `connect(2)` has succeeded and the application
                        // learns the rest from `read`/`write`, so this is not a refusal.
                        tcp::State::CloseWait
                        | tcp::State::FinWait1
                        | tcp::State::FinWait2
                        | tcp::State::Closing
                        | tcp::State::LastAck => Ok(()),
                        tcp::State::Closed | tcp::State::TimeWait => {
                            Err(ConnectError::InvalidState)
                        }
                        tcp::State::SynSent => Err(ConnectError::InProgress),
                        // Neither is reachable for a socket this process just `connect`ed, and
                        // none of them means "connected" -- a guest-reachable path must return an
                        // errno rather than panic here, so report the refusal the peer's RST (or
                        // the connect timeout) would have produced anyway.
                        tcp::State::Listen | tcp::State::SynReceived => {
                            Err(ConnectError::InvalidState)
                        }
                    }
                };

                // Stale-handle guard (see `socket_set_contains`'s own doc comment): a dead-holder
                // `reset_after_poisoning()` elsewhere may have wiped this handle out of
                // `socket_set` already -- report it the same way a genuinely closed/reset socket
                // would read, instead of panicking deep in smoltcp's own `get_mut`.
                if !Self::socket_set_contains(&self.socket_set, socket_handle.handle) {
                    return Err(ConnectError::InvalidState);
                }
                let socket: &mut tcp::Socket = self.socket_set.get_mut(socket_handle.handle);
                if check_progress {
                    check_state(socket.state())
                } else {
                    let local_port = self.local_port_allocator.ephemeral_port()?;
                    let local_endpoint: smoltcp::wire::IpListenEndpoint = local_port.port().into();
                    let addr: smoltcp::wire::IpEndpoint = (*addr).into();
                    match socket.connect(self.interface.context(), addr, local_endpoint) {
                        Ok(()) => {
                            socket.set_timeout(Some(TCP_CONNECT_TIMEOUT));
                            let tcp_specific = socket_handle.tcp_mut();
                            tcp_specific.connect_initiated_at_us = Some(now);
                            tcp_specific.connect_peer_port = Some(addr.port);
                            let old_port = tcp_specific.local_port.replace(local_port);
                            if old_port.is_some() {
                                // Need to think about how to handle this situation
                                unimplemented!()
                            }
                            check_state(socket.state())
                        }
                        Err(tcp::ConnectError::InvalidState) => unreachable!(),
                        Err(tcp::ConnectError::Unaddressable) => {
                            self.local_port_allocator.deallocate(local_port);
                            Err(ConnectError::Unaddressable)
                        }
                    }
                }
            }
            Protocol::Udp => {
                if addr.port() == 0 {
                    return Err(ConnectError::Unaddressable);
                }
                // Stale-handle guard -- see the identical TCP-branch guard above.
                if !Self::socket_set_contains(&self.socket_set, socket_handle.handle) {
                    return Err(ConnectError::InvalidState);
                }
                let socket: &mut udp::Socket = self.socket_set.get_mut(socket_handle.handle);
                if !socket.is_open() {
                    let local_port = self.local_port_allocator.ephemeral_port()?;
                    let local_endpoint: smoltcp::wire::IpListenEndpoint = local_port.port().into();
                    let Ok(()) = socket.bind(local_endpoint) else {
                        unreachable!("binding to a free port cannot fail")
                    };
                }
                let addr: smoltcp::wire::IpEndpoint = (*addr).into();
                socket_handle.udp_mut().remote_endpoint = Some(addr);
                Ok(())
            }
            Protocol::Icmp => unimplemented!(),
            Protocol::Raw { protocol: _ } => unimplemented!(),
        };

        let mut result = ret;
        if let Err(ref err) = ret {
            let what = match err {
                ConnectError::TimedOut => "timeout",
                ConnectError::Unaddressable => "unaddressable",
                ConnectError::InvalidState => "refused",
                ConnectError::InProgress => "in-progress",
                _ => "other",
            };
            report_connect_failure(what, addr.port());
        }
        if let Some(proxy) = &socket_handle.proxy {
            match ret {
                Ok(()) => proxy.set_state(socket_channel::SocketState::Connected),
                Err(ConnectError::InProgress) => {
                    proxy.set_state(socket_channel::SocketState::Connecting);
                }
                Err(ConnectError::Unaddressable) => {
                    proxy.set_async_error(errors::SocketAsyncError::ConnectionRefused);
                }
                Err(ConnectError::InvalidState) => {
                    // Distinguish timeout from RST using elapsed time
                    match socket_handle.tcp().connect_initiated_at_us {
                        Some(initiated_at) if now - initiated_at >= TCP_CONNECT_TIMEOUT => {
                            proxy.set_async_error(errors::SocketAsyncError::TimedOut);
                            result = Err(ConnectError::TimedOut);
                        }
                        _ => proxy.set_async_error(errors::SocketAsyncError::ConnectionRefused),
                    }
                }
                Err(_) => {}
            }
        }
        drop(table_entry);
        drop(descriptor_table);

        self.automated_platform_interaction(PollDirection::Both);
        result
    }

    pub fn get_local_addr(&self, fd: &SocketFd<Platform>) -> Result<SocketAddr, LocalAddrError> {
        let descriptor_table = self.litebox.descriptor_table();
        let mut table_entry = descriptor_table
            .get_entry_mut(fd)
            .ok_or(LocalAddrError::InvalidFd)?;
        let socket_handle = &mut table_entry.entry;

        // Stale-handle guard (see `socket_set_contains`'s own doc comment): a dead-holder
        // `reset_after_poisoning()` elsewhere may have wiped this handle out of `socket_set`
        // already. Reported the same way an unbound socket already is below (`Ipv4Addr::
        // UNSPECIFIED`, port 0) rather than panicking deep in smoltcp's own `get` -- a stale
        // handle genuinely has no address to report, same as a never-bound one.
        if !Self::socket_set_contains(&self.socket_set, socket_handle.handle) {
            return Ok(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)));
        }
        match socket_handle.protocol() {
            Protocol::Tcp => {
                // A bound or listening socket has no smoltcp `local_endpoint()` (it is only set
                // once a connection exists), so `getsockname()` used to report `0.0.0.0:0` for
                // every server: Python's `http.server` and Selkies both logged "port 0", and any
                // program that binds port 0 to learn its ephemeral port got the wrong answer.
                if let Some(server) = socket_handle.tcp().server_socket.as_ref() {
                    let endpoint = server.ip_listen_endpoint;
                    let ip = match endpoint.addr {
                        Some(smoltcp::wire::IpAddress::Ipv4(ipv4)) => ipv4,
                        None => Ipv4Addr::UNSPECIFIED,
                    };
                    return Ok(SocketAddr::V4(SocketAddrV4::new(ip, endpoint.port)));
                }
                let socket: &tcp::Socket = self.socket_set.get(socket_handle.handle);
                match socket.local_endpoint() {
                    Some(endpoint) => match endpoint.addr {
                        smoltcp::wire::IpAddress::Ipv4(ipv4) => {
                            Ok(SocketAddr::V4(SocketAddrV4::new(ipv4, endpoint.port)))
                        }
                    },
                    // Bound but not connected (a listener, or a socket that only called
                    // `bind`): smoltcp has no local endpoint yet, but the address the guest bound
                    // is recorded on our side and is what `getsockname` must report.
                    None => {
                        let specific = socket_handle.tcp();
                        if let Some(server) = &specific.server_socket {
                            let ip = match server.ip_listen_endpoint.addr {
                                Some(smoltcp::wire::IpAddress::Ipv4(ipv4)) => ipv4,
                                None => Ipv4Addr::UNSPECIFIED,
                            };
                            Ok(SocketAddr::V4(SocketAddrV4::new(
                                ip,
                                server.ip_listen_endpoint.port,
                            )))
                        } else if let Some(local_port) = &specific.local_port {
                            Ok(SocketAddr::V4(SocketAddrV4::new(
                                Ipv4Addr::UNSPECIFIED,
                                local_port.port(),
                            )))
                        } else {
                            Ok(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)))
                        }
                    }
                }
            }
            Protocol::Udp => {
                let socket: &udp::Socket = self.socket_set.get(socket_handle.handle);
                let local_endpoint = socket.endpoint();
                match local_endpoint.addr {
                    Some(smoltcp::wire::IpAddress::Ipv4(ipv4)) => {
                        Ok(SocketAddr::V4(SocketAddrV4::new(ipv4, local_endpoint.port)))
                    }
                    None => {
                        let source_address = socket_handle
                            .udp()
                            .remote_endpoint
                            .map_or(Ipv4Addr::UNSPECIFIED, |remote| match remote.addr {
                                smoltcp::wire::IpAddress::Ipv4(ip) if ip.is_loopback() => ip,
                                smoltcp::wire::IpAddress::Ipv4(_) => INTERFACE_IP_ADDR,
                            });
                        Ok(SocketAddr::V4(SocketAddrV4::new(
                            source_address,
                            local_endpoint.port,
                        )))
                    }
                }
            }
            Protocol::Icmp => unimplemented!(),
            Protocol::Raw { protocol: _ } => unimplemented!(),
        }
    }

    pub fn get_remote_addr(&self, fd: &SocketFd<Platform>) -> Result<SocketAddr, RemoteAddrError> {
        let descriptor_table = self.litebox.descriptor_table();
        let mut table_entry = descriptor_table
            .get_entry_mut(fd)
            .ok_or(RemoteAddrError::InvalidFd)?;
        let socket_handle = &mut table_entry.entry;
        self.get_remote_addr_for_handle(socket_handle)
    }

    fn get_remote_addr_for_handle(
        &self,
        socket_handle: &SocketHandle<Platform>,
    ) -> Result<SocketAddr, RemoteAddrError> {
        // Stale-handle guard (see `socket_set_contains`'s own doc comment) -- reported the same
        // way a real "never connected" TCP socket already is, rather than panicking deep in
        // smoltcp's own `get`.
        if !Self::socket_set_contains(&self.socket_set, socket_handle.handle) {
            return Err(RemoteAddrError::NotConnected);
        }
        let endpoint = match socket_handle.protocol() {
            Protocol::Tcp => self
                .socket_set
                .get::<tcp::Socket>(socket_handle.handle)
                .remote_endpoint()
                .ok_or(RemoteAddrError::NotConnected)?,
            Protocol::Udp => socket_handle
                .udp()
                .remote_endpoint
                .ok_or(RemoteAddrError::NotConnected)?,
            Protocol::Icmp => unimplemented!(),
            Protocol::Raw { protocol: _ } => unimplemented!(),
        };
        match endpoint.addr {
            smoltcp::wire::IpAddress::Ipv4(ipv4) => {
                Ok(SocketAddr::V4(SocketAddrV4::new(ipv4, endpoint.port)))
            }
        }
    }

    /// Bind a socket to a specific address and port. If the port is 0, an ephemeral port is allocated.
    pub fn bind(
        &mut self,
        fd: &SocketFd<Platform>,
        socket_addr: &SocketAddr,
    ) -> Result<(), BindError> {
        let SocketAddr::V4(addr) = socket_addr else {
            return Err(BindError::UnsupportedAddress(*socket_addr));
        };

        let descriptor_table = self.litebox.descriptor_table();
        let mut table_entry = descriptor_table
            .get_entry_mut(fd)
            .ok_or(BindError::InvalidFd)?;
        let socket_handle = &mut table_entry.entry;
        match socket_handle.protocol() {
            Protocol::Tcp => {
                if socket_handle.tcp().server_socket.is_some() {
                    return Err(BindError::AlreadyBound);
                }
                let lp = self
                    .local_port_allocator
                    .allocate_local_port(addr.port())
                    .map_err(|_| BindError::PortAlreadyInUse(addr.port()))?;
                let new_port = lp.port();
                let old_lp = socket_handle.tcp_mut().local_port.replace(lp);
                if let Some(old) = old_lp {
                    self.local_port_allocator.deallocate(old);
                    // Currently unsure if the dealloc is sufficient and if we need to do
                    // anything else here (possibly return an error message due to trying to
                    // do things to a connected socket, not sure), so just marking as
                    // unimplemented for now to trigger a panic.
                    unimplemented!()
                }
                // See the analogous comment in the `Protocol::Udp` arm below: an unspecified
                // address must map to `addr: None` (wildcard), not `Some(0.0.0.0)`, or inbound
                // SYNs addressed to the interface's real address will be rejected.
                let bind_addr = if addr.ip().is_unspecified() {
                    None
                } else {
                    Some(smoltcp::wire::IpAddress::Ipv4(*addr.ip()))
                };
                socket_handle.tcp_mut().server_socket = Some(TcpServerSpecific {
                    ip_listen_endpoint: smoltcp::wire::IpListenEndpoint {
                        addr: bind_addr,
                        port: new_port,
                    },
                    backlog: None,
                    socket_set_handles: vec![],
                    no_slot_listening_reported: false,
                });
            }
            Protocol::Udp => {
                let lp = self
                    .local_port_allocator
                    .allocate_local_port(addr.port())
                    .map_err(|_| BindError::PortAlreadyInUse(addr.port()))?;
                // An unspecified address (`0.0.0.0`) means "any local address" and must map to
                // `addr: None` (smoltcp's true wildcard), not `Some(0.0.0.0)`. `UdpSocket::accepts`
                // treats `Some(_)` as a literal address requiring an exact match against the
                // packet's real destination address, so `Some(0.0.0.0)` would silently reject every
                // inbound datagram whose destination is the interface's real address (e.g.
                // `10.0.0.2`) instead of matching everything as intended -- this broke every
                // reply to an auto-bound (unconnected `sendto()`) UDP socket, e.g. DNS queries.
                let bind_addr = if addr.ip().is_unspecified() {
                    None
                } else {
                    Some(smoltcp::wire::IpAddress::Ipv4(*addr.ip()))
                };
                let local_endpoint = smoltcp::wire::IpListenEndpoint {
                    addr: bind_addr,
                    port: lp.port(),
                };
                // Stale-handle guard (see `socket_set_contains`'s own doc comment) -- treated the
                // same as an invalid fd (the handle no longer names anything real to bind)
                // instead of panicking deep in smoltcp's own `get_mut`.
                if !Self::socket_set_contains(&self.socket_set, socket_handle.handle) {
                    self.local_port_allocator.deallocate(lp);
                    return Err(BindError::InvalidFd);
                }
                let socket: &mut udp::Socket = self.socket_set.get_mut(socket_handle.handle);
                if let Err(e) = socket.bind(local_endpoint) {
                    self.local_port_allocator.deallocate(lp);
                    return Err(match e {
                        udp::BindError::InvalidState => BindError::AlreadyBound,
                        udp::BindError::Unaddressable => unreachable!(),
                    });
                }
            }
            Protocol::Icmp => unimplemented!(),
            Protocol::Raw { protocol: _ } => unimplemented!(),
        }

        drop(table_entry);
        drop(descriptor_table);

        self.automated_platform_interaction(PollDirection::Both);
        Ok(())
    }

    /// Shut down part of a full-duplex connection (`shutdown(2)`).
    ///
    /// # Why this exists
    ///
    /// `shutdown(SHUT_WR)` is how a server signals "my response is complete" without closing the
    /// socket: it sends a FIN while leaving the read half open. Essentially every HTTP server does
    /// this at the end of a response -- Python's `http.server`, nginx, and websockify included --
    /// so a stack that refuses it leaves the client waiting forever for bytes that were already
    /// written. Confirmed live before this existed: a Python `http.server` inside the guest logged
    /// `"GET / HTTP/1.1" 200` for a real host request, and the host client still timed out with
    /// zero bytes received, because `shutdown` returned `EOPNOTSUPP` and the response was never
    /// terminated.
    ///
    /// smoltcp models only the write half explicitly: `tcp::Socket::close()` sends the FIN, which
    /// is exactly `SHUT_WR`. There is no separate "stop receiving" operation on a smoltcp socket
    /// (a real kernel's `SHUT_RD` only affects local delivery, sending nothing on the wire), so
    /// `SHUT_RD` is accepted and has no wire effect -- matching what a caller observes on Linux,
    /// rather than failing a request this stack can honestly satisfy.
    pub fn shutdown(
        &mut self,
        fd: &SocketFd<Platform>,
        write_half: bool,
    ) -> Result<(), ShutdownError> {
        let descriptor_table = self.litebox.descriptor_table();
        let mut table_entry = descriptor_table
            .get_entry_mut(fd)
            .ok_or(ShutdownError::InvalidFd)?;
        let socket_handle = &mut table_entry.entry;
        if !write_half {
            // `SHUT_RD` alone: nothing to emit on the wire (see this function's doc comment).
            return Ok(());
        }
        match &socket_handle.specific {
            ProtocolSpecific::Tcp(_) => {
                // Stale-handle guard (see `socket_set_contains`'s own doc comment): nothing left
                // to send a FIN on, so this is a harmless no-op instead of panicking deep in
                // smoltcp's own `get_mut`.
                if Self::socket_set_contains(&self.socket_set, socket_handle.handle) {
                    // Mark the write side shut down and let the drain step send the FIN once the
                    // bytes still queued in the TX buffer have gone out; `close()` here (smoltcp's
                    // *send a FIN* operation, NOT a teardown: the socket stays in the set and the
                    // read half keeps delivering until the peer closes too) discarded them.
                    // Releasing the fd remains `close_handle`'s job, unchanged.
                    if let Some(socket_channel::NetworkProxy::Stream(channel)) =
                        socket_handle.proxy.as_deref()
                    {
                        channel.shutdown_write();
                        let now = self.now();
                        let shared_across_fork = self.is_shared_across_fork(socket_handle.handle);
                        Self::drain_socket_channel_buffers(
                            &mut self.socket_set,
                            socket_handle,
                            now,
                            shared_across_fork,
                            false,
                            &self.accepted_slots,
                        );
                    } else {
                        let pending_in_channel = socket_handle
                            .proxy
                            .as_ref()
                            .is_some_and(|proxy| proxy.has_pending_tx());
                        let tcp_socket: &mut tcp::Socket =
                            self.socket_set.get_mut(socket_handle.handle);
                        if pending_in_channel
                            || (tcp_socket.may_send() && tcp_socket.send_queue() > 0)
                        {
                            socket_handle.shutdown_wr_pending = true;
                        } else {
                            tcp_socket.close();
                        }
                    }
                }
                Ok(())
            }
            // UDP/ICMP/raw are connectionless: real Linux reports `ENOTCONN` for `shutdown` on a
            // socket that was never connected, which is what a caller can actually act on.
            ProtocolSpecific::Udp(_) | ProtocolSpecific::Icmp(_) | ProtocolSpecific::Raw(_) => {
                Err(ShutdownError::NotConnected)
            }
        }
    }

    /// Prepare a socket to accept incoming connections. Marks the socket as a passive socket, such
    /// that it will be used to accept new connection requests via [`accept`](Self::accept).
    ///
    /// The `backlog` argument defines the maximum length to which the queue of pending connections
    /// the `fd` may grow. This function is allowed to silently cap the value to a reasonable upper
    /// bound.
    pub fn listen(&mut self, fd: &SocketFd<Platform>, backlog: u16) -> Result<(), ListenError> {
        let descriptor_table = self.litebox.descriptor_table();
        let mut table_entry = descriptor_table
            .get_entry_mut(fd)
            .ok_or(ListenError::InvalidFd)?;
        let socket_handle = &mut table_entry.entry;
        // Real Linux treats a `listen(fd, 0)` backlog as a request for the minimum usable queue
        // depth, not an error -- `nginx` (and other servers configuring a modest worker count)
        // legitimately calls `listen()` this way. Match that by flooring to 1 rather than
        // panicking, mirroring the `.min(8)` upper-bound clamp just below.
        let backlog = backlog.max(1);

        // This prevents users from overloading things too badly; 4096 is the upper limit with
        // similar silent-cap behavior since Linux 5.4 (earlier versions capped even smaller, at
        // 128, but we use the larger value to be more flexible).
        //
        // TODO: smoltcp performs a linear search through SocketSet when dispatching an incoming
        // packet to the socket it belongs to, so having a large backlog can cause performance issues
        // (see https://github.com/smoltcp-rs/smoltcp/issues/973). Restricting the backlog to a smaller
        // value for now until we have a better solution.
        let backlog = backlog.min(8);

        match &mut socket_handle.specific {
            ProtocolSpecific::Tcp(handle) => {
                if handle.server_socket.is_none() {
                    let local_port =
                        self.local_port_allocator
                            .ephemeral_port()
                            .map_err(|e| match e {
                                local_ports::LocalPortAllocationError::AlreadyInUse(_) => {
                                    unreachable!()
                                }
                                local_ports::LocalPortAllocationError::NoAvailableFreePorts => {
                                    ListenError::NoAvailableFreeEphemeralPorts
                                }
                            })?;
                    let port = local_port.port();
                    let old_local_port = handle.local_port.replace(local_port);
                    if let Some(lp) = old_local_port {
                        self.local_port_allocator.deallocate(lp);
                        // Should anything else be done here?
                        unimplemented!()
                    }
                    handle.server_socket = Some(TcpServerSpecific {
                        ip_listen_endpoint: smoltcp::wire::IpListenEndpoint {
                            addr: Some(smoltcp::wire::IpAddress::v4(0, 0, 0, 0)),
                            port,
                        },
                        backlog: None,
                        socket_set_handles: vec![],
                        no_slot_listening_reported: false,
                    });
                }
                let Some(server_socket) = &mut handle.server_socket else {
                    unreachable!()
                };
                if server_socket.ip_listen_endpoint.port == 0 {
                    return Err(ListenError::InvalidAddress);
                }
                if server_socket.backlog.is_some() || !server_socket.socket_set_handles.is_empty() {
                    // Real servers (nginx's master process included) legitimately call `listen()`
                    // again on an already-listening socket -- most commonly to grow the backlog,
                    // but Linux also permits shrinking it. Growing just needs more pending-accept
                    // sockets queued (handled below by `refill_to_backlog`); shrinking drops the
                    // excess still-unconnected listening sockets from the tail of the list, since
                    // those are equivalent placeholders with no client-visible state yet.
                    let new_backlog_usize: usize = backlog.into();
                    if server_socket.socket_set_handles.len() > new_backlog_usize {
                        for handle in server_socket
                            .socket_set_handles
                            .split_off(new_backlog_usize)
                        {
                            // Stale-handle guard (see `socket_set_contains`'s own doc comment) --
                            // a handle already wiped by a dead-holder `reset_after_poisoning()`
                            // elsewhere has nothing left to remove.
                            if Self::socket_set_contains(&self.socket_set, handle) {
                                let _ = Self::remove_socket(&mut self.socket_set, &mut self.buffers, handle);
                            }
                        }
                    }
                    server_socket.backlog = Some(backlog);
                } else {
                    server_socket.backlog = Some(backlog);
                    server_socket.socket_set_handles = Vec::with_capacity(backlog.into());
                }
                server_socket.refill_to_backlog(&mut self.socket_set, &mut self.buffers);
                // Whoever arms a port's accept queue is the process that has to keep arming it:
                // recorded so a fork child that inherits the port can tell an owner that is GONE
                // from one that is merely quiet, and adopt the queue instead of sitting on a port
                // that answers nothing for the rest of the session.
                Self::record_listen_owner(
                    &self.listen_owner,
                    server_socket.ip_listen_endpoint.port,
                    self.litebox.platform().current_pid(),
                );
            }
            ProtocolSpecific::Udp(_) => unimplemented!(),
            ProtocolSpecific::Icmp(_) => unimplemented!(),
            ProtocolSpecific::Raw(_) => unimplemented!(),
        }

        if let Some(proxy) = &socket_handle.proxy {
            proxy.set_state(socket_channel::SocketState::Listening);
        }

        drop(table_entry);
        drop(descriptor_table);

        self.automated_platform_interaction(PollDirection::Ingress);
        Ok(())
    }

    /// Accept a new incoming connection on a listening socket.
    ///
    /// If `peer` is provided, it is filled with the remote address of the accepted connection.
    ///
    /// Note that the returned new socket has no associated proxy; to set a proxy, use
    /// [`set_socket_proxy`](Self::set_socket_proxy).
    pub fn accept(
        &mut self,
        fd: &SocketFd<Platform>,
        peer: Option<&mut SocketAddr>,
    ) -> Result<SocketFd<Platform>, AcceptError> {
        self.automated_platform_interaction(PollDirection::Both);
        let descriptor_table = self.litebox.descriptor_table();
        let mut table_entry = descriptor_table
            .get_entry_mut(fd)
            .ok_or(AcceptError::InvalidFd)?;
        let socket_handle = &mut table_entry.entry;
        match &mut socket_handle.specific {
            ProtocolSpecific::Tcp(handle) => {
                let Some(server_socket) = &mut handle.server_socket else {
                    return Err(AcceptError::NotListening);
                };
                if server_socket.backlog.is_none() {
                    return Err(AcceptError::NotListening);
                }
                // (Purely an optimization) remove all handles that are closed, by only keeping ones
                // that are not closed. A stale handle (see `socket_set_contains`'s own doc
                // comment: a dead-holder `reset_after_poisoning()` elsewhere may have wiped it out
                // of `socket_set` already) is treated the same as a closed one -- both get
                // dropped here -- instead of panicking deep in smoltcp's own `get`, live-caught
                // (twenty-eighth pass) as a real `"handle does not refer to a valid socket"` panic
                // that killed a whole cross-process-fork child's guest-execution thread outright
                // (this was selkies' own `accept()` call).
                let handles_before_retain = server_socket.socket_set_handles.len();
                server_socket.socket_set_handles.retain(|&h| {
                    Self::socket_set_contains(&self.socket_set, h) && {
                        let socket: &tcp::Socket = self.socket_set.get(h);
                        socket.is_open()
                    }
                });
                // A backlog slot dropped here is a socket nobody is listening on any more, so the
                // port must be re-armed to its backlog in BOTH arms: with every handle gone the
                // port has NO socket left listening, every later SYN is refused, no handle can
                // ever become `Established` again, and so `drain_socket_channel_buffers`'s
                // readable re-arm never fires either -- the port stays dead for the rest of the
                // session while the connections it already accepted keep working (chrD92: an
                // in-guest `curl 127.0.0.1:8081` was refused from t=120s to the end of the run
                // while selkies' accepted websocket went on streaming).  The success arm refills
                // after its own `swap_remove` below; this is the other one.
                let stale_dropped = handles_before_retain - server_socket.socket_set_handles.len();
                if stale_dropped > 0 {
                    litebox_util_log::warn!(
                        dropped = stale_dropped;
                        "diag-accept: listening backlog slot(s) went stale, re-arming the listener"
                    );
                    server_socket.refill_to_backlog(&mut self.socket_set, &mut self.buffers);
                }
                let own_ready = {
                    // A claim outlives the connection it marked only until the slot is armed back
                    // into LISTEN, and smoltcp reuses a freed slot index, so claims are reaped
                    // before any scan -- otherwise one stale mark strands the next connection
                    // that lands on that slot (neither scan would offer it to anybody).
                    Self::reap_stale_claims_in(&self.socket_set, &mut self.accepted_slots);
                    server_socket.socket_set_handles.iter().position(|&h| {
                    Self::socket_set_contains(&self.socket_set, h) && {
                        let socket: &tcp::Socket = self.socket_set.get(h);
                        // Linux hands a connection out of the accept queue even once its peer's FIN
                        // has landed (`CloseWait`): the application gets the fd and learns the peer
                        // is gone by reading EOF. Requiring `Established` here stranded such a slot
                        // -- `is_open()` keeps it, so the retain above never drops it, no later
                        // `accept` matches it, and the refill below only runs when a slot actually
                        // leaves, so one peer-closed connection cost the port a backlog slot
                        // forever and after `backlog` of them the port refused every SYN for the
                        // rest of the session (fl6: `LAST_FORK_WHERE_8095_ANSWERED=7`, backlog 8).
                        matches!(
                            socket.state(),
                            tcp::State::Established | tcp::State::CloseWait
                        )
                    }
                    // A slot another process of this fork family already took is not pending here
                    // (see `Network::accepted_slots`): parent and child accept from ONE queue.
                    && !Self::is_accepted_in(&self.accepted_slots, h)
                })
                };
                let ready_handle = match own_ready {
                    Some(position) => {
                        let ready_handle = server_socket.socket_set_handles.swap_remove(position);
                        Self::mark_accepted_in(&mut self.accepted_slots, ready_handle);
                        server_socket.refill_to_backlog(&mut self.socket_set, &mut self.buffers);
                        ready_handle
                    }
                    // Nothing in this process's own backlog: a connection can still be queued on
                    // the endpoint itself, armed there by whichever process owns this port's
                    // listening slots -- which is the only state a fork child sharing its parent's
                    // listener ever sees, since it arms none of its own. The borrower never
                    // refills: adding slots here would be arming a SECOND listener on the port.
                    None => {
                        Self::reap_stale_claims_in(&self.socket_set, &mut self.accepted_slots);
                        let Some(ready_handle) = Self::unclaimed_connection_on(
                            &self.socket_set,
                            &self.accepted_slots,
                            server_socket.ip_listen_endpoint.port,
                        ) else {
                            if let Some(proxy) = &socket_handle.proxy {
                                proxy.set_readable(false);
                            }
                            return Err(AcceptError::NoConnectionsReady);
                        };
                        Self::mark_accepted_in(&mut self.accepted_slots, ready_handle);
                        ready_handle
                    }
                };
                if let Some(proxy) = &socket_handle.proxy {
                    // reset the readable flag so that we send one [`Events::In`] event per accepted connection
                    proxy.set_readable(false);
                }
                let local_port = handle
                    .local_port
                    .as_ref()
                    .map(|lp| self.local_port_allocator.allocate_same_local_port(lp));
                drop(table_entry);
                drop(descriptor_table);
                let handle = SocketHandle {
                    consider_closed: core::sync::atomic::AtomicBool::new(false),
                    shutdown_wr_pending: false,
                    handle: ready_handle,
                    specific: ProtocolSpecific::Tcp(TcpSpecific {
                        local_port,
                        server_socket: None,
                        immediate_close: AtomicBool::new(false),
                        connect_initiated_at_us: None,
                        connect_peer_port: None,
                    }),
                    proxy: None,
                    // `closing_in_background` retires an accepted connection's own socket once its
                    // FIN exchange finishes; removing it here would drop it mid-close.
                    borrowed: false,
                    own_slot: false,
                };
                if let Some(peer) = peer {
                    let Ok(remote_addr) = self.get_remote_addr_for_handle(&handle) else {
                        unreachable!("a connected TCP socket must have a remote address")
                    };
                    *peer = remote_addr;
                }
                Ok(self.new_socket_fd_for(handle))
            }
            ProtocolSpecific::Udp(_) => unimplemented!(),
            ProtocolSpecific::Icmp(_) => unimplemented!(),
            ProtocolSpecific::Raw(_) => unimplemented!(),
        }
    }

    /// Send data over a socket, optionally specifying the destination address.
    ///
    /// If the socket is connection-mode and the destination address is provided,
    /// `Err(SendError::UnnecessaryDestinationAddress)` is returned.
    pub fn send(
        &mut self,
        fd: &SocketFd<Platform>,
        buf: &[u8],
        flags: SendFlags,
        destination: Option<SocketAddr>,
    ) -> Result<usize, SendError> {
        let descriptor_table = self.litebox.descriptor_table();
        let mut table_entry = descriptor_table
            .get_entry_mut(fd)
            .ok_or(SendError::InvalidFd)?;
        let socket_handle = &mut table_entry.entry;
        if !flags.is_empty() {
            unimplemented!()
        }

        let ret = match socket_handle.protocol() {
            Protocol::Tcp => {
                if destination.is_some() {
                    return Err(SendError::UnnecessaryDestinationAddress);
                }
                // Stale-handle guard (see `socket_set_contains`'s own doc comment) -- reported
                // the same way smoltcp's own `tcp::SendError::InvalidState` already is just
                // below, rather than panicking deep in its `get_mut`.
                if !Self::socket_set_contains(&self.socket_set, socket_handle.handle) {
                    return Err(SendError::SocketInInvalidState);
                }
                self.socket_set
                    .get_mut::<tcp::Socket>(socket_handle.handle)
                    .send_slice(buf)
                    .map_err(|tcp::SendError::InvalidState| SendError::SocketInInvalidState)
            }
            Protocol::Udp => {
                let destination = destination
                    .map(|s| match s {
                        SocketAddr::V4(addr) => smoltcp::wire::IpEndpoint::from(addr),
                        SocketAddr::V6(_) => unimplemented!(),
                    })
                    .or_else(|| socket_handle.udp().remote_endpoint);
                let Some(remote_endpoint) = destination else {
                    return Err(SendError::DestinationAddressRequired);
                };
                if !Self::socket_set_contains(&self.socket_set, socket_handle.handle) {
                    return Err(SendError::SocketInInvalidState);
                }
                let udp_socket: &mut udp::Socket = self.socket_set.get_mut(socket_handle.handle);
                if !udp_socket.is_open() {
                    let local_port = self
                        .local_port_allocator
                        .ephemeral_port()
                        .map_err(SendError::PortAllocationFailure)?;
                    let port = local_port.port();
                    let Ok(()) =
                        udp_socket.bind(smoltcp::wire::IpListenEndpoint { addr: None, port })
                    else {
                        self.local_port_allocator.deallocate(local_port);
                        unreachable!("binding to a free port cannot fail")
                    };
                }
                udp_socket
                    .send_slice(buf, remote_endpoint)
                    .map(|()| buf.len())
                    .map_err(|e| match e {
                        udp::SendError::BufferFull => SendError::BufferFull,
                        udp::SendError::Unaddressable => SendError::Unaddressable,
                    })
            }
            Protocol::Icmp => unimplemented!(),
            Protocol::Raw { protocol: _ } => unimplemented!(),
        };

        drop(table_entry);
        drop(descriptor_table);

        self.automated_platform_interaction(PollDirection::Egress);
        ret
    }

    /// Receive data from a connected socket.
    ///
    /// If the `source_addr` is `Some` and the underlying protocol provides a source address, it will be updated.
    /// e.g., UDP does provide the source address, while TCP does not (because it is connection-oriented,
    /// once it is established, both ends should already know each other's addresses).
    ///
    /// On success, returns the number of bytes received.
    pub fn receive(
        &mut self,
        fd: &SocketFd<Platform>,
        buf: &mut [u8],
        flags: ReceiveFlags,
        source_addr: Option<&mut Option<SocketAddr>>,
    ) -> Result<usize, ReceiveError> {
        // Note that we do an earlier-than-usual automated interaction to ingress packets since it
        // doesn't hurt to do this too often (other than wasting energy), and this allows us to
        // possibly get packets where we might otherwise return with size 0 on the `receive`.
        self.automated_platform_interaction(PollDirection::Ingress);
        let descriptor_table = self.litebox.descriptor_table();
        let mut table_entry = descriptor_table
            .get_entry_mut(fd)
            .ok_or(ReceiveError::InvalidFd)?;
        let socket_handle = &mut table_entry.entry;
        if flags.intersects(
            (ReceiveFlags::DONTWAIT | ReceiveFlags::TRUNC | ReceiveFlags::DISCARD).complement(),
        ) {
            unimplemented!("flags: {:?}", flags);
        }

        let ret = match socket_handle.protocol() {
            Protocol::Tcp => {
                if let Some(source_addr) = source_addr {
                    *source_addr = None;
                }
                // Stale-handle guard (see `socket_set_contains`'s own doc comment) -- reported
                // the same way smoltcp's own `tcp::RecvError::InvalidState` already is just
                // below, rather than panicking deep in its `get_mut`.
                if !Self::socket_set_contains(&self.socket_set, socket_handle.handle) {
                    return Err(ReceiveError::SocketInInvalidState);
                }
                let tcp_socket = self.socket_set.get_mut::<tcp::Socket>(socket_handle.handle);
                if flags.contains(ReceiveFlags::TRUNC) {
                    unimplemented!("TRUNC flag for tcp");
                }
                if flags.contains(ReceiveFlags::DISCARD) {
                    let discard_slice =
                        |tcp_socket: &mut tcp::Socket<'_>| -> Result<usize, tcp::RecvError> {
                            // See [`tcp::Socket::recv_slice`] and [`tcp::Socket::recv`] for why we do two `recv` calls.
                            // Basically, the socket buffer is implemented as a ring buffer, and if the data to be read
                            // wraps around, a single `recv` call will not be able to read all the data.
                            let size1 = tcp_socket.recv(|data| (data.len(), data.len()))?;
                            let size2 = tcp_socket.recv(|data| (data.len(), data.len()))?;
                            Ok(size1 + size2)
                        };
                    discard_slice(tcp_socket)
                } else {
                    tcp_socket.recv_slice(buf)
                }
                .map_err(|e| match e {
                    tcp::RecvError::InvalidState => ReceiveError::SocketInInvalidState,
                    tcp::RecvError::Finished => ReceiveError::OperationFinished,
                })
            }
            Protocol::Udp => {
                // Stale-handle guard -- see the identical TCP-branch guard above.
                if !Self::socket_set_contains(&self.socket_set, socket_handle.handle) {
                    return Err(ReceiveError::SocketInInvalidState);
                }
                let udp_socket = self.socket_set.get_mut::<udp::Socket>(socket_handle.handle);
                match udp_socket.recv() {
                    Ok((data, meta)) => {
                        if let Some(source_addr) = source_addr {
                            let remote_addr = match meta.endpoint.addr {
                                smoltcp::wire::IpAddress::Ipv4(ipv4_addr) => {
                                    SocketAddr::V4(SocketAddrV4::new(ipv4_addr, meta.endpoint.port))
                                }
                            };
                            *source_addr = Some(remote_addr);
                        }
                        let n = if flags.contains(ReceiveFlags::DISCARD) {
                            data.len()
                        } else {
                            let length = data.len().min(buf.len());
                            buf[..length].copy_from_slice(&data[..length]);
                            if flags.contains(ReceiveFlags::TRUNC) {
                                // return the real size of the packet or datagram,
                                // even when it was longer than the passed buffer.
                                data.len()
                            } else {
                                length
                            }
                        };
                        Ok(n)
                    }
                    Err(udp::RecvError::Exhausted) => Ok(0),
                    Err(udp::RecvError::Truncated) => unreachable!(),
                }
            }
            Protocol::Icmp => unimplemented!(),
            Protocol::Raw { protocol: _ } => unimplemented!(),
        };

        drop(table_entry);
        drop(descriptor_table);

        self.automated_platform_interaction(PollDirection::Ingress);
        ret
    }

    pub fn set_tcp_option(
        &mut self,
        fd: &SocketFd<Platform>,
        data: TcpOptionData,
    ) -> Result<(), errors::SetTcpOptionError> {
        let descriptor_table = self.litebox.descriptor_table();
        let mut table_entry = descriptor_table
            .get_entry_mut(fd)
            .ok_or(errors::SetTcpOptionError::InvalidFd)?;
        let socket_handle = &mut table_entry.entry;
        match socket_handle.protocol() {
            Protocol::Tcp => {
                // Stale-handle guard (see `socket_set_contains`'s own doc comment) -- reported as
                // an invalid fd (the handle no longer names anything real) instead of panicking
                // deep in smoltcp's own `get_mut`.
                if !Self::socket_set_contains(&self.socket_set, socket_handle.handle) {
                    return Err(errors::SetTcpOptionError::InvalidFd);
                }
                let tcp_socket = self.socket_set.get_mut::<tcp::Socket>(socket_handle.handle);
                match data {
                    TcpOptionData::NODELAY(nodelay) => {
                        tcp_socket.set_nagle_enabled(!nodelay);
                    }
                    TcpOptionData::KEEPALIVE(keepalive) => {
                        tcp_socket.set_keep_alive(keepalive.map(smoltcp::time::Duration::from));
                    }
                    TcpOptionData::CONGESTION(congestion) => match congestion {
                        CongestionControl::None => {
                            tcp_socket.set_congestion_control(tcp::CongestionControl::None);
                        }
                        _ => unimplemented!(),
                    },
                }
                Ok(())
            }
            Protocol::Udp | Protocol::Icmp | Protocol::Raw { .. } => {
                Err(errors::SetTcpOptionError::NotTcpSocket)
            }
        }
    }
    pub fn get_tcp_option(
        &self,
        fd: &SocketFd<Platform>,
        name: TcpOptionName,
    ) -> Result<TcpOptionData, errors::GetTcpOptionError> {
        let descriptor_table = self.litebox.descriptor_table();
        let mut table_entry = descriptor_table
            .get_entry_mut(fd)
            .ok_or(errors::GetTcpOptionError::InvalidFd)?;
        let socket_handle = &mut table_entry.entry;
        match socket_handle.protocol() {
            Protocol::Tcp => {
                // Stale-handle guard -- see the identical guard in `set_tcp_option` above.
                if !Self::socket_set_contains(&self.socket_set, socket_handle.handle) {
                    return Err(errors::GetTcpOptionError::InvalidFd);
                }
                let tcp_socket = self.socket_set.get::<tcp::Socket>(socket_handle.handle);
                match name {
                    TcpOptionName::NODELAY => {
                        Ok(TcpOptionData::NODELAY(!tcp_socket.nagle_enabled()))
                    }
                    TcpOptionName::KEEPALIVE => Ok(TcpOptionData::KEEPALIVE(
                        tcp_socket.keep_alive().map(core::time::Duration::from),
                    )),
                    TcpOptionName::CONGESTION => Ok(TcpOptionData::CONGESTION(
                        match tcp_socket.congestion_control() {
                            tcp::CongestionControl::None => CongestionControl::None,
                        },
                    )),
                }
            }
            Protocol::Udp | Protocol::Icmp | Protocol::Raw { .. } => {
                Err(errors::GetTcpOptionError::NotTcpSocket)
            }
        }
    }
}

/// Protocols for sockets supported by LiteBox
#[non_exhaustive]
pub enum Protocol {
    Tcp,
    Udp,
    Icmp,
    Raw { protocol: u8 },
}

bitflags! {
    /// Flags for the `receive` function.
    #[derive(Clone, Copy, Debug)]
    pub struct ReceiveFlags: u32 {
        /// `MSG_CMSG_CLOEXEC`: close-on-exec for the associated file descriptor
        const CMSG_CLOEXEC = 0x40000000;
        /// `MSG_DONTWAIT`: non-blocking operation
        const DONTWAIT = 0x40;
        /// `MSG_ERRQUEUE`: destination for error messages
        const ERRQUEUE = 0x2000;
        /// `MSG_OOB`: requests receipt of out-of-band data
        const OOB = 0x1;
        /// `MSG_PEEK`: requests to peek at incoming messages
        const PEEK = 0x2;
        /// `MSG_TRUNC`: truncate the message
        const TRUNC = 0x20;
        /// `MSG_WAITALL`: wait for the full amount of data
        const WAITALL = 0x100;
        /// Discard the received data
        const DISCARD = 0x8000;
    }
}

bitflags! {
    /// Flags for the `send` function.
    #[derive(Clone, Copy, Debug)]
    pub struct SendFlags: u32 {
        /// `MSG_CONFIRM`: requests confirmation of the message delivery.
        const CONFIRM = 0x800;
        /// `MSG_DONTROUTE`: send the message directly to the interface, bypassing routing.
        const DONTROUTE = 0x4;
        /// `MSG_DONTWAIT`: non-blocking operation, do not wait for buffer space to become available.
        const DONTWAIT = 0x40;
        /// `MSG_EOR`: indicates the end of a record for message-oriented sockets.
        const EOR = 0x80;
        /// `MSG_MORE`: indicates that more data will follow.
        const MORE = 0x8000;
        /// `MSG_NOSIGNAL`: prevents the sending of SIGPIPE signals when writing to a socket that is closed.
        const NOSIGNAL = 0x4000;
        /// `MSG_OOB`: sends out-of-band data.
        const OOB = 0x1;
    }
}

/// Socket options for TCP
#[non_exhaustive]
pub enum TcpOptionName {
    /// If set, disable the Nagle algorithm. This means that
    /// segments are always sent as soon as possible, even if there
    /// is only a small amount of data.
    NODELAY,
    /// Enable sending of keep-alive messages.
    KEEPALIVE,
    /// TCP congestion control algorithm
    CONGESTION,
}

/// Data for TCP options
///
/// Note it should be paired with the correct [`TcpOptionName`] variant.
/// For example, `TcpOptionName::NODELAY` should be paired with `TcpOptionData::NODELAY(true)`.
#[non_exhaustive]
pub enum TcpOptionData {
    NODELAY(bool),
    KEEPALIVE(Option<core::time::Duration>),
    CONGESTION(CongestionControl),
}

/// TCP Congestion Control Algorithms
#[non_exhaustive]
pub enum CongestionControl {
    None,
    Reno,
    Cubic,
}

#[derive(Debug, Clone, Copy)]
pub enum CloseBehavior {
    /// Close the socket immediately (i.e., abortive close).
    Immediate,
    /// Close the socket in background and return immediately
    Graceful,
    /// Close the socket in background only if there is not unsent data remaining,
    /// else return an error.
    GracefulIfNoPendingData,
}

/// Encodes an IPv4 address for [`Network::fork_carry_spec`]'s spec; `0` for "unspecified"/absent,
/// which is this stack's only address family (see `Network::new`'s interface addresses).
fn v4_to_u32(addr: impl Into<Option<smoltcp::wire::IpAddress>>) -> u32 {
    match Into::<Option<smoltcp::wire::IpAddress>>::into(addr) {
        Some(smoltcp::wire::IpAddress::Ipv4(v4)) => u32::from_be_bytes(v4.octets()),
        _ => 0,
    }
}

/// The inverse of [`v4_to_u32`], for [`Network::fork_adopt`].
fn u32_to_v4(addr: u32) -> smoltcp::wire::Ipv4Address {
    smoltcp::wire::Ipv4Address::from_octets(u32::to_be_bytes(addr))
}

crate::fd::enable_fds_for_subsystem! {
    @Platform: { platform::IPInterfaceProvider + platform::TimeProvider + sync::RawSyncPrimitivesProvider + platform::SharedKernelStateProvider };
    Network<Platform>;
    @Platform: { platform::TimeProvider + sync::RawSyncPrimitivesProvider };
    SocketHandle<Platform>;
    -> SocketFd<Platform>;
}
