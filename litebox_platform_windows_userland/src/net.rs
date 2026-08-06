// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Real IP-packet networking for the Windows userland platform, via an in-process userspace NAT
//! gateway -- **no Administrator privileges, no driver, no virtual adapter required**.
//!
//! LiteBox's own network stack (`litebox::net`, backed by `smoltcp` configured with
//! `medium-ip`) sends and receives raw IP packets to/from the guest's virtual interface
//! (`10.0.0.2/24`, gateway `10.0.0.1`) via [`litebox::platform::IPInterfaceProvider`]. On Linux,
//! `litebox_platform_linux_userland` hands those packets to a real kernel TUN device
//! (`/dev/net/tun`), and relies on the *host* having IP forwarding and NAT (`MASQUERADE`)
//! configured to actually reach the Internet.
//!
//! On Windows, the equivalent of a TUN device (Wintun) requires Administrator privileges to
//! create a virtual adapter -- acceptable for a one-time host setup step on Linux, but not for a
//! per-invocation requirement of a standalone, unprivileged Windows executable. Raw sockets
//! (`SOCK_RAW`) and WinDivert both have the same elevation requirement (creating a raw socket or
//! loading a WFP callout driver are both privileged operations on Windows), so no "real network
//! interface" style backend can satisfy "runs as a normal user."
//!
//! Instead, this module implements a **userspace NAT gateway**: a second, private `smoltcp`
//! `Interface` plays the role of the gateway (`10.0.0.1`) that the guest's own `smoltcp` instance
//! already targets. It is configured in IP-router mode (`Interface::set_any_ip(true)` plus a
//! default route whose next-hop is itself), so it accepts packets addressed to *any* destination
//! IP, not just its own -- exactly the behavior of a router/NAT gateway. For each new TCP
//! connection or UDP flow the guest opens, this module opens a **real, unprivileged Winsock
//! socket** (via `std`/`socket2`, using ordinary `connect()`/`send()`/`recv()` -- the same API any
//! desktop application uses) to the real destination, and pumps bytes between the guest-facing
//! smoltcp socket and the real OS socket. This is the same architectural approach used by QEMU's
//! `-netdev user` (slirp), Docker Desktop's networking, and gVisor's netstack: a full IP stack is
//! terminated in userspace and re-emitted as ordinary client socket calls, so from the host
//! kernel's point of view, LiteBox looks like any other unprivileged process making outbound
//! connections -- nothing about it requires elevation.
//!
//! # Scope
//!
//! Only outbound (guest-initiated) TCP and UDP flows are proxied; the guest acting as a TCP/UDP
//! *server* reachable from the real network is out of scope (this mirrors what a NAT gateway with
//! no configured port-forwarding rules provides -- the common case for `apk`/`wget`/`curl`-style
//! outbound-only workloads). ICMP (`ping`) is not proxied.

use std::collections::HashMap;
use std::io::{ErrorKind, Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    HardwareAddress, IpAddress, IpCidr, IpListenEndpoint, IpProtocol, Ipv4Packet, TcpPacket,
    UdpPacket,
};

/// IP address of LiteBox's guest-side virtual interface. Must stay in sync with
/// `litebox::net::INTERFACE_IP_ADDR`.
const GUEST_IP_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);

/// IP address of the gateway that this module implements. Must stay in sync with
/// `litebox::net::GATEWAY_IP_ADDR`.
const GATEWAY_IP_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);

/// Maximum transmission unit, matching `litebox::net::phy::DEVICE_MTU`.
const DEVICE_MTU: usize = 1600;

/// TCP/UDP socket buffer sizes for the gateway-side sockets.
const SOCKET_BUFFER_SIZE: usize = 65536 * 4;

/// Common destination ports the gateway pre-listens on so a guest's first SYN to one of them
/// doesn't need to wait for a listening-socket refill. Anything else still works via
/// [`GatewayState::ensure_listening`], just with one extra poll round-trip on first use.
const WELL_KNOWN_PORTS: &[u16] = &[53, 80, 443];

/// How long a UDP NAT flow (`GatewayState::udp_flows`) may sit with no traffic in either
/// direction before `pump_udp` reaps it.
///
/// UDP is connectionless, so unlike TCP there is no FIN/close handshake that tells the gateway
/// "the guest is done with this flow" -- the only removal trigger before this timeout existed was
/// a hard error from the real socket's `recv_from` (e.g. `ECONNRESET`), which a normal, successful
/// one-shot exchange (the overwhelmingly common case: a single DNS query/response on port 53)
/// never produces. Every such exchange therefore left its `UdpFlow` (and the real, ephemeral-port
/// `UdpSocket` bound in `pump_udp`'s `or_insert_with`) permanently resident in `udp_flows` for the
/// rest of the process's lifetime -- confirmed in practice: after enough real DNS lookups within
/// one `litebox_runner_linux_on_windows_userland.exe` invocation (e.g. partway through `apk add
/// nodejs`'s per-package mirror lookups), fresh outbound connections started stalling
/// indefinitely, and the same symptom was independently reproducible via repeated `wget` calls
/// against unrelated hosts in the same process -- consistent with cumulative ephemeral-port/socket
/// exhaustion rather than a per-request bug. A 30s idle timeout is generous for DNS (whose
/// exchanges complete in milliseconds) while still bounding the leak.
const UDP_FLOW_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Max number of concurrently backlogged listening sockets per port (allows a handful of
/// simultaneous new connections to the same port without dropping SYNs).
const LISTEN_BACKLOG_PER_PORT: usize = 4;

/// A minimal in-process, thread-safe packet queue used as the "wire" between the guest's
/// `smoltcp` interface (driven by `litebox::net::Network`, calling
/// [`send_ip_packet`]/[`receive_ip_packet`]) and this module's private gateway-side `smoltcp`
/// interface. This replaces what would otherwise be a real NIC or TUN device: both directions are
/// just raw IP packets handed off in memory, since both ends are `smoltcp` instances running in
/// the same process.
#[derive(Default)]
struct LoopbackQueue {
    /// Packets sent by the guest, waiting to be processed by the gateway.
    to_gateway: std::collections::VecDeque<Vec<u8>>,
    /// Packets sent by the gateway (or proxied replies), waiting to be delivered to the guest.
    to_guest: std::collections::VecDeque<Vec<u8>>,
}

/// `smoltcp::phy::Device` implementation for the gateway-side interface, backed by
/// [`LoopbackQueue`].
struct GatewayDevice {
    queue: Arc<Mutex<LoopbackQueue>>,
}

impl Device for GatewayDevice {
    type RxToken<'a> = GatewayRxToken;
    type TxToken<'a> = GatewayTxToken;

    fn receive(
        &mut self,
        _timestamp: SmolInstant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut q = self.queue.lock().unwrap();
        let packet = q.to_gateway.pop_front()?;
        Some((
            GatewayRxToken { packet },
            GatewayTxToken {
                queue: self.queue.clone(),
            },
        ))
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(GatewayTxToken {
            queue: self.queue.clone(),
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = DEVICE_MTU;
        caps
    }
}

struct GatewayRxToken {
    packet: Vec<u8>,
}
impl RxToken for GatewayRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.packet)
    }
}

struct GatewayTxToken {
    queue: Arc<Mutex<LoopbackQueue>>,
}
impl TxToken for GatewayTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let res = f(&mut buf);
        self.queue.lock().unwrap().to_guest.push_back(buf);
        res
    }
}

/// State for one proxied TCP flow: a gateway-side `smoltcp` socket (terminating the guest's TCP
/// connection) bridged to a real, unprivileged OS `TcpStream` connected to the real destination.
struct TcpFlow {
    /// `Connecting` while a background thread is running `Socket::connect_timeout` (a real
    /// blocking connect with a bounded wait, which correctly detects connection-refused/timeout
    /// on Windows -- unlike hand-rolled nonblocking-connect + `getpeername()` polling, which
    /// proved unreliable: `getpeername()` can report success before the TCP handshake actually
    /// finishes, leading to premature writes that fail with `WSAENOTCONN`).
    state: TcpFlowState,
    /// The real socket returned EOF or errored; only draining remaining smoltcp-side data.
    real_closed: bool,
    /// Bytes read from the guest but not yet fully written to `real` (a nonblocking write can
    /// legitimately write fewer bytes than requested, or return `WouldBlock` entirely).
    pending_to_real: Vec<u8>,
    /// Bytes read from `real` but not yet fully enqueued into the guest-facing smoltcp socket
    /// (`Socket::send_slice` can likewise enqueue fewer bytes than given when its TX buffer is
    /// full).
    pending_to_guest: Vec<u8>,
}

enum TcpFlowState {
    Connecting(std::sync::mpsc::Receiver<std::io::Result<std::net::TcpStream>>),
    Connected(std::net::TcpStream),
}

/// State for one proxied UDP flow, keyed by the guest's source port: a real OS `UdpSocket`
/// that redirects each datagram to whatever destination the guest most recently sent to (matching
/// how a NAT UDP "connection" tracks the most recent 5-tuple).
struct UdpFlow {
    real: std::net::UdpSocket,
    /// Last time a datagram was sent or received on this flow. Used by `pump_udp` to reap flows
    /// idle longer than [`UDP_FLOW_IDLE_TIMEOUT`] -- see that constant's doc comment for why this
    /// is necessary (UDP has no close signal to remove a flow otherwise).
    last_active: std::time::Instant,
}

/// The gateway-side networking state: the private `smoltcp` interface/socket-set, plus the NAT
/// flow tables bridging accepted connections to real OS sockets.
struct GatewayState {
    device: GatewayDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    /// Listening TCP sockets, keyed by (port, backlog slot). Refilled as connections are accepted.
    tcp_listeners: HashMap<(u16, usize), SocketHandle>,
    /// Active proxied TCP flows, keyed by the accepted socket's handle.
    tcp_flows: HashMap<SocketHandle, TcpFlow>,
    /// UDP sockets bound wildcard-address (any destination IP) but *not* wildcard-port -- unlike
    /// TCP, `smoltcp`'s UDP `accepts()` always requires an exact destination-port match (`port:
    /// 0` is rejected by `bind` outright), so one socket per destination port is required. Keyed
    /// by destination port; created on demand the first time the guest sends to a new port.
    udp_listeners: HashMap<u16, SocketHandle>,
    /// Active UDP NAT flows, keyed by (destination port, guest source port).
    udp_flows: HashMap<(u16, u16), UdpFlow>,
    zero_time: std::time::Instant,
}

impl GatewayState {
    fn new(queue: Arc<Mutex<LoopbackQueue>>) -> Self {
        let mut device = GatewayDevice { queue };
        let config = Config::new(HardwareAddress::Ip);
        let mut iface = Interface::new(config, &mut device, SmolInstant::ZERO);
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::Ipv4(GATEWAY_IP_ADDR), 24))
                .unwrap();
        });
        // Router mode: accept packets addressed to any destination, not just our own IP(s).
        iface.set_any_ip(true);
        // A default route whose gateway is ourselves satisfies the route-lookup check that
        // `any_ip` mode still performs (see `smoltcp`'s `iface/interface/ipv4.rs`): any
        // destination resolves to a route whose next-hop is one of our own addresses, so the
        // packet is accepted instead of silently dropped.
        iface
            .routes_mut()
            .add_default_ipv4_route(GATEWAY_IP_ADDR)
            .unwrap();

        let mut sockets = SocketSet::new(vec![]);

        let mut tcp_listeners = HashMap::new();
        for &port in WELL_KNOWN_PORTS {
            for slot in 0..LISTEN_BACKLOG_PER_PORT {
                let handle = new_listening_tcp_socket(&mut sockets, port);
                tcp_listeners.insert((port, slot), handle);
            }
        }

        let mut udp_listeners = HashMap::new();
        for &port in WELL_KNOWN_PORTS {
            let handle = new_wildcard_udp_socket(&mut sockets, port);
            udp_listeners.insert(port, handle);
        }

        Self {
            device,
            iface,
            sockets,
            tcp_listeners,
            tcp_flows: HashMap::new(),
            udp_listeners,
            udp_flows: HashMap::new(),
            zero_time: std::time::Instant::now(),
        }
    }

    fn now(&self) -> SmolInstant {
        SmolInstant::from_micros_const(
            i64::try_from(self.zero_time.elapsed().as_micros()).unwrap_or(i64::MAX),
        )
    }

    /// Ensure there's a listening socket ready for `port`, creating one on demand for ports
    /// outside [`WELL_KNOWN_PORTS`]. Cheap no-op if one already exists.
    fn ensure_listening(&mut self, port: u16) {
        if self.tcp_listeners.contains_key(&(port, 0)) {
            return;
        }
        let handle = new_listening_tcp_socket(&mut self.sockets, port);
        self.tcp_listeners.insert((port, 0), handle);
    }

    /// Inspect packets the guest has queued but not yet processed, and make sure a listening
    /// socket exists for whatever destination TCP/UDP port they target -- a listening socket
    /// must already exist *before* `Interface::poll` processes an inbound SYN or UDP datagram for
    /// smoltcp to accept it, so this must run ahead of `poll()`.
    fn ensure_listeners_for_queued_packets(&mut self) {
        let queued: Vec<Vec<u8>> = {
            let q = self.device.queue.lock().unwrap();
            q.to_gateway.iter().cloned().collect()
        };
        for packet in queued {
            let Ok(ipv4) = Ipv4Packet::new_checked(&packet) else {
                continue;
            };
            match ipv4.next_header() {
                IpProtocol::Tcp => {
                    if let Ok(tcp) = TcpPacket::new_checked(ipv4.payload()) {
                        self.ensure_listening(tcp.dst_port());
                    }
                }
                IpProtocol::Udp => {
                    if let Ok(udp) = UdpPacket::new_checked(ipv4.payload()) {
                        self.ensure_udp_listening(udp.dst_port());
                    }
                }
                _ => {}
            }
        }
    }

    /// One round of: poll the gateway interface, accept newly-established TCP connections
    /// (spawning their real-socket bridge), pump bytes for all active TCP/UDP flows, and refill
    /// any listening sockets that got consumed by an accepted connection.
    fn drive(&mut self) {
        self.ensure_listeners_for_queued_packets();

        let now = self.now();
        self.iface.poll(now, &mut self.device, &mut self.sockets);

        // Accept newly-established connections on any listening socket.
        let listener_handles: Vec<((u16, usize), SocketHandle)> =
            self.tcp_listeners.iter().map(|(&k, &v)| (k, v)).collect();
        for ((port, slot), handle) in listener_handles {
            let established = {
                let socket: &tcp::Socket = self.sockets.get(handle);
                socket.state() == tcp::State::Established
            };
            if established {
                self.tcp_listeners.remove(&(port, slot));
                self.accept_tcp_flow(handle, port);
                // Refill so future connections to this port can still be accepted.
                let new_handle = new_listening_tcp_socket(&mut self.sockets, port);
                self.tcp_listeners.insert((port, slot), new_handle);
            }
        }

        self.pump_tcp_flows();
        self.pump_udp();

        // A second poll to flush any smoltcp-side sends queued up by the pumps above.
        let now = self.now();
        self.iface.poll(now, &mut self.device, &mut self.sockets);
    }

    /// Begin proxying a freshly-accepted TCP connection: look up its real destination (the
    /// "local" endpoint from the gateway's perspective, since the guest dialed it as if the
    /// gateway itself were the destination) and open a real, unprivileged outbound socket to it.
    ///
    /// The actual `connect()` runs on a short-lived background thread using a real *blocking*
    /// connect with a bounded timeout (`Socket::connect_timeout`), rather than a hand-rolled
    /// nonblocking-connect-then-poll loop: on Windows, polling `getpeername()` to detect
    /// completion is unreliable (it can report success before the handshake actually finishes,
    /// causing the very first write to fail with `WSAENOTCONN`), whereas a blocking connect with
    /// `select()`-based completion detection is both correct and well-tested. Running it off the
    /// gateway's single driver thread keeps that thread from stalling on slow/unreachable
    /// destinations.
    fn accept_tcp_flow(&mut self, handle: SocketHandle, listen_port: u16) {
        let dest = {
            let socket: &tcp::Socket = self.sockets.get(handle);
            socket.local_endpoint()
        };
        let Some(dest) = dest else {
            return;
        };
        let IpAddress::Ipv4(dest_ip) = dest.addr;
        let dest_addr = SocketAddr::V4(SocketAddrV4::new(dest_ip, listen_port));

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| -> std::io::Result<std::net::TcpStream> {
                let socket = socket2::Socket::new(
                    socket2::Domain::IPV4,
                    socket2::Type::STREAM,
                    Some(socket2::Protocol::TCP),
                )?;
                socket.connect_timeout(&dest_addr.into(), Duration::from_secs(10))?;
                socket.set_nonblocking(true)?;
                Ok(socket.into())
            })();
            let _ = tx.send(result);
        });

        self.tcp_flows.insert(
            handle,
            TcpFlow {
                state: TcpFlowState::Connecting(rx),
                real_closed: false,
                pending_to_real: Vec::new(),
                pending_to_guest: Vec::new(),
            },
        );
    }

    /// Pump bytes for every active TCP flow: guest -> real socket, and real socket -> guest.
    fn pump_tcp_flows(&mut self) {
        let mut to_remove = Vec::new();
        for (&handle, flow) in &mut self.tcp_flows {
            if let TcpFlowState::Connecting(rx) = &flow.state {
                match rx.try_recv() {
                    Ok(Ok(stream)) => flow.state = TcpFlowState::Connected(stream),
                    Ok(Err(_)) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        flow.real_closed = true;
                        continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => continue,
                }
            }
            let TcpFlowState::Connected(real) = &mut flow.state else {
                unreachable!("just transitioned out of Connecting above, or continued out")
            };

            let socket: &mut tcp::Socket = self.sockets.get_mut(handle);

            if !flow.real_closed {
                // guest -> real. `real` is a nonblocking socket, so a short/`WouldBlock` write
                // must be retried later rather than treated via `write_all` (which errors out on
                // `WouldBlock` instead of buffering/retrying) -- any unwritten remainder is kept
                // in `pending_to_real` and retried on the next `drive()` cycle before reading any
                // further guest data, to preserve TCP byte-stream ordering.
                if !flow.pending_to_real.is_empty() {
                    match real.write(&flow.pending_to_real) {
                        Ok(n) => {
                            flow.pending_to_real.drain(..n);
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                        Err(_) => flow.real_closed = true,
                    }
                }
                while flow.pending_to_real.is_empty() && socket.can_recv() {
                    let mut buf = [0u8; 4096];
                    let n = socket
                        .recv_slice(&mut buf)
                        .unwrap_or_default()
                        .min(buf.len());
                    if n == 0 {
                        break;
                    }
                    match real.write(&buf[..n]) {
                        Ok(written) if written == n => {}
                        Ok(written) => {
                            flow.pending_to_real.extend_from_slice(&buf[written..n]);
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            flow.pending_to_real.extend_from_slice(&buf[..n]);
                        }
                        Err(_) => {
                            flow.real_closed = true;
                            break;
                        }
                    }
                }
                // real -> guest. `flow.pending_to_guest` holds bytes read from `real` but not yet
                // fully enqueued into the smoltcp socket's TX buffer: `Socket::send_slice` is a
                // byte-stream partial-write, exactly like a real `write()`, and can enqueue fewer
                // bytes than given when its buffer is full -- silently dropping the remainder
                // (as `let _ = socket.send_slice(...)` previously did) corrupted the byte stream
                // whenever the guest's receive window/backlog fell behind a fast real-socket
                // reader, exactly the kind of truncation `apk`'s "I/O error" pointed at.
                if !flow.pending_to_guest.is_empty() && socket.can_send() {
                    match socket.send_slice(&flow.pending_to_guest) {
                        Ok(n) => {
                            flow.pending_to_guest.drain(..n);
                        }
                        Err(_) => flow.real_closed = true,
                    }
                }
                while flow.pending_to_guest.is_empty() && socket.can_send() {
                    let mut buf = [0u8; 4096];
                    match real.read(&mut buf) {
                        Ok(0) => {
                            flow.real_closed = true;
                            break;
                        }
                        Ok(n) => match socket.send_slice(&buf[..n]) {
                            Ok(sent) if sent == n => {}
                            Ok(sent) => {
                                flow.pending_to_guest.extend_from_slice(&buf[sent..n]);
                            }
                            Err(_) => {
                                flow.real_closed = true;
                                break;
                            }
                        },
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(_) => {
                            flow.real_closed = true;
                            break;
                        }
                    }
                }
            }

            // Only close once every byte read from `real` has actually been handed to the guest:
            // `pending_to_guest` non-empty means some already-read real-socket bytes are still
            // waiting on smoltcp TX buffer space, and `send_queue() > 0` means smoltcp itself
            // hasn't finished delivering them to the guest yet -- closing early in either case
            // would truncate the response the guest sees.
            if flow.real_closed && flow.pending_to_guest.is_empty() && socket.send_queue() == 0 {
                socket.close();
            }
            if !socket.is_open() {
                to_remove.push(handle);
            }
        }
        for handle in to_remove {
            self.tcp_flows.remove(&handle);
            self.sockets.remove(handle);
        }
    }

    /// Ensure there's a wildcard-address UDP socket listening for `dest_port`, creating one on
    /// demand for ports outside [`WELL_KNOWN_PORTS`].
    fn ensure_udp_listening(&mut self, dest_port: u16) -> SocketHandle {
        *self
            .udp_listeners
            .entry(dest_port)
            .or_insert_with(|| new_wildcard_udp_socket(&mut self.sockets, dest_port))
    }

    /// Pump datagrams for every UDP destination port the guest has talked to: each inbound
    /// datagram from the guest is relayed via a real `UdpSocket` keyed by (destination port,
    /// guest source port), and any reply read back from that real socket is relayed back to the
    /// guest, addressed as if it came from the real destination.
    fn pump_udp(&mut self) {
        // First, drain any newly-created listeners' pending guest datagrams and open/refresh the
        // matching real-socket flow.
        let dest_ports: Vec<u16> = self.udp_listeners.keys().copied().collect();
        for dest_port in dest_ports {
            let handle = self.udp_listeners[&dest_port];
            let socket: &mut udp::Socket = self.sockets.get_mut(handle);
            while socket.can_recv() {
                let Ok((data, meta)) = socket.recv() else {
                    break;
                };
                let guest_port = meta.endpoint.port;
                let Some(IpAddress::Ipv4(dest_ip)) = meta.local_address else {
                    continue;
                };
                let dest_addr = SocketAddrV4::new(dest_ip, dest_port);

                let flow = self
                    .udp_flows
                    .entry((dest_port, guest_port))
                    .or_insert_with(|| {
                        let real = std::net::UdpSocket::bind("0.0.0.0:0")
                            .expect("failed to bind ephemeral UDP socket");
                        let _ = real.set_nonblocking(true);
                        UdpFlow {
                            real,
                            last_active: std::time::Instant::now(),
                        }
                    });
                flow.last_active = std::time::Instant::now();
                let _ = flow.real.send_to(data, SocketAddr::V4(dest_addr));
            }
        }

        // Drain replies for every active UDP flow back to the guest.
        let mut dead_flows = Vec::new();
        for (&(dest_port, guest_port), flow) in &mut self.udp_flows {
            let Some(&handle) = self.udp_listeners.get(&dest_port) else {
                continue;
            };
            let socket: &mut udp::Socket = self.sockets.get_mut(handle);
            let mut buf = [0u8; 4096];
            loop {
                match flow.real.recv_from(&mut buf) {
                    Ok((n, SocketAddr::V4(from))) => {
                        flow.last_active = std::time::Instant::now();
                        if !socket.can_send() {
                            break;
                        }
                        let meta = udp::UdpMetadata {
                            endpoint: smoltcp::wire::IpEndpoint {
                                addr: IpAddress::Ipv4(GUEST_IP_ADDR),
                                port: guest_port,
                            },
                            local_address: Some(IpAddress::Ipv4(*from.ip())),
                            meta: smoltcp::phy::PacketMeta::default(),
                        };
                        let _ = socket.send_slice(&buf[..n], meta);
                    }
                    Ok((_, SocketAddr::V6(_))) => {}
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        if flow.last_active.elapsed() >= UDP_FLOW_IDLE_TIMEOUT {
                            dead_flows.push((dest_port, guest_port));
                        }
                        break;
                    }
                    Err(_) => {
                        dead_flows.push((dest_port, guest_port));
                        break;
                    }
                }
            }
        }
        for key in dead_flows {
            self.udp_flows.remove(&key);
        }
    }
}

fn new_wildcard_udp_socket(sockets: &mut SocketSet<'static>, port: u16) -> SocketHandle {
    let mut socket = udp::Socket::new(
        smoltcp::storage::PacketBuffer::new(
            vec![smoltcp::storage::PacketMetadata::EMPTY; 32],
            vec![0u8; SOCKET_BUFFER_SIZE],
        ),
        smoltcp::storage::PacketBuffer::new(
            vec![smoltcp::storage::PacketMetadata::EMPTY; 32],
            vec![0u8; SOCKET_BUFFER_SIZE],
        ),
    );
    // Wildcard *address* (accept datagrams sent to any destination IP), but the destination
    // *port* must match exactly -- `smoltcp` has no wildcard-port bind for UDP.
    socket
        .bind(IpListenEndpoint { addr: None, port })
        .expect("bind on a freshly-created UDP socket cannot fail for a nonzero port");
    sockets.add(socket)
}

fn new_listening_tcp_socket(sockets: &mut SocketSet<'static>, port: u16) -> SocketHandle {
    let mut socket = tcp::Socket::new(
        smoltcp::storage::RingBuffer::new(vec![0u8; SOCKET_BUFFER_SIZE]),
        smoltcp::storage::RingBuffer::new(vec![0u8; SOCKET_BUFFER_SIZE]),
    );
    // `addr: None` is the wildcard: accept connections whose *destination* is any address, not
    // just our own -- this is what makes the gateway transparently proxy to arbitrary real
    // destinations rather than only accepting connections to `10.0.0.1` itself.
    socket
        .listen(IpListenEndpoint { addr: None, port })
        .expect("listen on a freshly-created socket cannot fail");
    sockets.add(socket)
}

/// Shared handle to the gateway state plus the loopback queue used to exchange packets with the
/// guest-facing `litebox::net::Network` (via [`send_ip_packet`]/[`receive_ip_packet`]).
///
/// Owned by [`crate::WindowsUserland`] as a lazily-initialized field (`OnceLock<NatGateway>`)
/// rather than a module-level `static`: this codebase actively ratchets down new global mutable
/// state (see `dev_tests/src/ratchet.rs`'s `ratchet_globals`), so a genuinely process-scoped
/// singleton like this belongs on the one process-lifetime `WindowsUserland` instance instead of
/// introducing another bare `static`.
pub(crate) struct NatGateway {
    queue: Arc<Mutex<LoopbackQueue>>,
    /// Signaled whenever a new packet is pushed into `queue.to_guest`, so
    /// [`wait_on_tun`] can sleep without busy-polling.
    notify: Arc<std::sync::Condvar>,
    notify_lock: Arc<Mutex<()>>,
}

impl NatGateway {
    fn new() -> Self {
        let queue = Arc::new(Mutex::new(LoopbackQueue::default()));
        let notify = Arc::new(std::sync::Condvar::new());
        let notify_lock = Arc::new(Mutex::new(()));

        let gateway_queue = queue.clone();
        let gateway_notify = notify.clone();
        std::thread::Builder::new()
            .name("litebox-nat-gateway".into())
            .spawn(move || {
                let mut state = GatewayState::new(gateway_queue);
                loop {
                    state.drive();
                    gateway_notify.notify_all();
                    // Bound the idle sleep so real-socket readiness (which nothing here wakes us
                    // for directly, since these are plain blocking-free `std` sockets polled by
                    // hand rather than through an OS readiness API) is still noticed promptly.
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
            .expect("failed to spawn NAT gateway thread");

        litebox_util_log::info!(
            "Userspace NAT gateway ready (no Administrator privileges required): guest side {GUEST_IP_ADDR}, gateway side {GATEWAY_IP_ADDR}"
        );

        Self {
            queue,
            notify,
            notify_lock,
        }
    }
}

/// Get (initializing on first use) the [`NatGateway`] behind `slot`.
///
/// Lazy so that non-networked invocations (e.g. `/bin/true`) never pay the cost of spinning up
/// the gateway thread. `slot` is a field on [`crate::WindowsUserland`], not a module-level
/// `static`; see [`NatGateway`]'s doc comment for why.
fn gateway(slot: &OnceLock<NatGateway>) -> &NatGateway {
    slot.get_or_init(NatGateway::new)
}

/// Send a raw IP packet from the guest into the NAT gateway.
///
/// Always succeeds: the packet is simply enqueued for the gateway thread to process on its next
/// cycle. The `Result` return type is fixed by `IPInterfaceProvider::send_ip_packet`, which
/// currently has no failure variants to report backpressure through.
#[allow(
    clippy::unnecessary_wraps,
    reason = "return type is fixed by the IPInterfaceProvider trait"
)]
pub(crate) fn send_ip_packet(
    slot: &OnceLock<NatGateway>,
    packet: &[u8],
) -> Result<(), litebox::platform::SendError> {
    let gw = gateway(slot);
    gw.queue
        .lock()
        .unwrap()
        .to_gateway
        .push_back(packet.to_vec());
    Ok(())
}

/// Attempt to receive a raw IP packet (originating from the NAT gateway, e.g. a proxied TCP/UDP
/// reply) without blocking.
pub(crate) fn receive_ip_packet(
    slot: &OnceLock<NatGateway>,
    packet: &mut [u8],
) -> Result<usize, litebox::platform::ReceiveError> {
    let gw = gateway(slot);
    let mut q = gw.queue.lock().unwrap();
    let Some(data) = q.to_guest.pop_front() else {
        return Err(litebox::platform::ReceiveError::WouldBlock);
    };
    let n = data.len().min(packet.len());
    packet[..n].copy_from_slice(&data[..n]);
    Ok(n)
}

/// Block the calling thread until either a packet is available to read from the gateway, or
/// `timeout` elapses. Mirrors `LinuxUserland::wait_on_tun`'s role for the network-worker thread.
pub(crate) fn wait_on_tun(slot: &OnceLock<NatGateway>, timeout: Option<core::time::Duration>) {
    let gw = gateway(slot);
    let has_packet = || !gw.queue.lock().unwrap().to_guest.is_empty();
    if has_packet() {
        return;
    }
    let guard = gw.notify_lock.lock().unwrap();
    let timeout = timeout.unwrap_or(Duration::from_millis(50));
    let _ = gw.notify.wait_timeout(guard, timeout);
}
