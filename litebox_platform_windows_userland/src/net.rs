// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Real IP-packet networking for the Windows userland platform, via an in-process userspace NAT
//! gateway -- **no Administrator privileges, no driver, no virtual adapter required**.
//!
//! The guest's `smoltcp` stack (`litebox::net`, `medium-ip`) exchanges raw IP packets with this
//! module through [`LoopbackQueue`]. A private second `smoltcp` `Interface` plays the gateway
//! (`10.0.0.1`) the guest already routes through and re-emits each flow as an ordinary
//! unprivileged Winsock socket to the real destination. Wintun, `SOCK_RAW` and WinDivert all
//! require elevation to install or attach on Windows, so no real-interface backend can satisfy
//! "runs as a normal user".
//!
//! # Scope
//!
//! Outbound (guest-initiated) TCP and UDP flows are proxied transparently. Inbound reachability
//! is opt-in and TCP-only: each `LITEBOX_PUBLISH=host:guest` entry (the runner's `--publish`,
//! mirroring `docker run -p`) binds a real `127.0.0.1:<host_port>` listener and forwards every
//! accepted host connection to `GUEST_IP_ADDR:<guest_port>`. No mapping exists by default.
//! Inbound UDP forwarding and ICMP are not implemented.

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

const SOCKET_BUFFER_SIZE: usize = 65536 * 4;

/// Destination ports given a listening socket up front, so a guest's first SYN to one of them is
/// accepted on the first poll rather than waiting for [`GatewayState::ensure_listening`].
const PRE_LISTENED_PORTS: &[u16] = &[53, 80, 443];

/// How long a UDP NAT flow (`GatewayState::udp_flows`) may sit with no traffic in either
/// direction before `pump_udp` reaps it: UDP has no FIN, so a flow whose exchange completes
/// normally is never removed any other way.
const UDP_FLOW_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Max number of concurrently backlogged listening sockets per port (allows a handful of
/// simultaneous new connections to the same port without dropping SYNs).
const LISTEN_BACKLOG_PER_PORT: usize = 4;

/// The "wire" between the guest's `smoltcp` interface and this module's gateway-side one: both
/// ends are `smoltcp` instances in this process, so a real NIC or TUN device is just two
/// in-memory raw-IP queues.
#[derive(Default)]
struct LoopbackQueue {
    /// Packets sent by the guest, waiting to be processed by the gateway.
    to_gateway: std::collections::VecDeque<Vec<u8>>,
    /// Packets sent by the gateway (or proxied replies), waiting to be delivered to the guest.
    to_guest: std::collections::VecDeque<Vec<u8>>,
}

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

/// One proxied TCP flow: a gateway-side `smoltcp` socket terminating the guest's connection,
/// bridged to a real, unprivileged OS `TcpStream` to the real destination.
struct TcpFlow {
    state: TcpFlowState,
    /// Set when the real socket hit EOF or an error; only the drain to the guest continues.
    real_eof_or_error: bool,
    pending_to_real: Vec<u8>,
    pending_to_guest: Vec<u8>,
    /// Set once `real`'s write half has been shut down in response to the smoltcp-side peer
    /// closing first, so that happens exactly once instead of on every `drive()` cycle.
    graceful_fin_sent: bool,
}

enum TcpFlowState {
    /// A background thread is running `Socket::connect_timeout`: a blocking connect with a
    /// bounded wait, which detects refused/timeout correctly. Polling `getpeername()` on a
    /// nonblocking connect does not -- it can report success before the handshake finishes, and
    /// the first write then fails with `WSAENOTCONN`.
    Connecting(std::sync::mpsc::Receiver<std::io::Result<std::net::TcpStream>>),
    Connected(std::net::TcpStream),
}

/// State for one proxied UDP flow, keyed by the guest's source port: a real OS `UdpSocket`
/// that redirects each datagram to whatever destination the guest most recently sent to (matching
/// how a NAT UDP "connection" tracks the most recent 5-tuple).
struct UdpFlow {
    real: std::net::UdpSocket,
    /// Last datagram sent or received; `pump_udp` reaps flows idle past [`UDP_FLOW_IDLE_TIMEOUT`].
    last_active: std::time::Instant,
}

/// The gateway-side networking state: the private `smoltcp` interface/socket-set, plus the NAT
/// flow tables bridging accepted connections to real OS sockets.
struct GatewayState {
    device: GatewayDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    /// Listening TCP sockets, keyed by (port, backlog slot), refilled as connections are accepted.
    tcp_listeners: HashMap<(u16, usize), SocketHandle>,
    tcp_flows: HashMap<SocketHandle, TcpFlow>,
    /// UDP sockets bound wildcard-address but *not* wildcard-port: `smoltcp`'s UDP `accepts()`
    /// always requires an exact destination-port match (`port: 0` is rejected by `bind`
    /// outright), so one socket per destination port is required. Keyed by destination port.
    udp_listeners: HashMap<u16, SocketHandle>,
    udp_flows: HashMap<(u16, u16), UdpFlow>,
    zero_time: std::time::Instant,
    /// Host-accepted connections from published ports, waiting to be bridged into the guest.
    /// `None` when no port is published, which is the common case.
    inbound_rx: Option<std::sync::mpsc::Receiver<InboundConnection>>,
    /// Next ephemeral source port for a gateway->guest connection. Collisions with a still-live
    /// flow are retried on the next cycle, since a `connect` on an in-use port fails cleanly
    /// rather than corrupting anything.
    next_ephemeral_port: u16,
    /// Gateway-side local ports owned by an INBOUND (published-port) flow's own connecting
    /// socket. These must never get a wildcard listening socket: the guest's replies on such a
    /// flow are addressed *to* this port, and a wildcard listener opened on it would compete
    /// with the real connecting socket for those very packets, delivering them to a socket
    /// nothing pumps.
    ports_claimed_by_inbound_flows: std::collections::HashSet<u16>,
    /// Which local port each inbound flow owns, so the claim above is released when that flow is
    /// reaped -- otherwise a long-lived process leaks one port per connection served.
    inbound_flow_ports: HashMap<SocketHandle, u16>,
}

impl GatewayState {
    fn new(
        queue: Arc<Mutex<LoopbackQueue>>,
        inbound_rx: Option<std::sync::mpsc::Receiver<InboundConnection>>,
    ) -> Self {
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
        // `any_ip` mode still performs a route lookup, so a default route whose next-hop is one
        // of our own addresses is what keeps every destination routable instead of dropped.
        iface
            .routes_mut()
            .add_default_ipv4_route(GATEWAY_IP_ADDR)
            .unwrap();

        let mut sockets = SocketSet::new(vec![]);

        let mut tcp_listeners = HashMap::new();
        for &port in PRE_LISTENED_PORTS {
            for slot in 0..LISTEN_BACKLOG_PER_PORT {
                let handle = new_listening_tcp_socket(&mut sockets, port);
                tcp_listeners.insert((port, slot), handle);
            }
        }

        let mut udp_listeners = HashMap::new();
        for &port in PRE_LISTENED_PORTS {
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
            inbound_rx,
            next_ephemeral_port: 49152,
            ports_claimed_by_inbound_flows: std::collections::HashSet::new(),
            inbound_flow_ports: HashMap::new(),
        }
    }

    /// Bridge every host connection accepted by a published-port listener into the guest, as an
    /// ordinary [`TcpFlow`] in the same table the outbound path uses -- already `Connected`, since
    /// the host peer's socket is accepted and there is nothing to connect *to* on the real side.
    /// Inbound forwarding then reuses the outbound byte pump rather than duplicating it.
    fn accept_inbound_flows(&mut self) {
        let Some(rx) = &self.inbound_rx else {
            return;
        };
        // Drain without blocking: this runs on the single gateway thread.
        let pending: Vec<InboundConnection> = rx.try_iter().collect();
        for conn in pending {
            let local_port = self.next_ephemeral_port;
            self.next_ephemeral_port = if self.next_ephemeral_port >= 65535 {
                49152
            } else {
                self.next_ephemeral_port + 1
            };
            let Some(handle) = new_connecting_tcp_socket(
                &mut self.sockets,
                &mut self.iface,
                conn.guest_port,
                local_port,
            ) else {
                // Port collision or socket-set exhaustion: drop this connection rather than
                // stalling the gateway. The host peer sees a closed connection and can retry.
                // Measured shape: the host peer's connect dies with no answer (`curl` code 000)
                // while the guest's own loopback connect to the same port still answers 200 --
                // chrF27, three HOLD ticks wide.
                litebox_util_log::warn!(
                    guest_port = conn.guest_port,
                    local_port = local_port,
                    flows = self.tcp_flows.len(),
                    sockets = self.sockets.iter().count(),
                    claimed_ports = self.ports_claimed_by_inbound_flows.len();
                    "diag-inbound-drop: a host connection could not be bridged into the guest"
                );
                continue;
            };
            self.ports_claimed_by_inbound_flows.insert(local_port);
            self.inbound_flow_ports.insert(handle, local_port);
            self.tcp_flows.insert(
                handle,
                TcpFlow {
                    state: TcpFlowState::Connected(conn.real),
                    real_eof_or_error: false,
                    pending_to_real: Vec::new(),
                    pending_to_guest: Vec::new(),
                    graceful_fin_sent: false,
                },
            );
        }
    }

    fn now(&self) -> SmolInstant {
        SmolInstant::from_micros_const(
            i64::try_from(self.zero_time.elapsed().as_micros()).unwrap_or(i64::MAX),
        )
    }

    fn ensure_listening(&mut self, port: u16) {
        if self.tcp_listeners.contains_key(&(port, 0)) {
            return;
        }
        let handle = new_listening_tcp_socket(&mut self.sockets, port);
        self.tcp_listeners.insert((port, 0), handle);
    }

    /// Whether an inbound flow owns `port`, meaning no wildcard listener may be opened on it --
    /// see `ports_claimed_by_inbound_flows`.
    fn port_is_claimed_by_an_inbound_flow(&self, port: u16) -> bool {
        self.ports_claimed_by_inbound_flows.contains(&port)
    }

    /// Make sure a listening socket exists for whatever destination TCP/UDP port the guest's
    /// queued packets target: `smoltcp` only accepts a SYN or datagram if a listening socket was
    /// already bound before `Interface::poll` processes it, so this runs ahead of `poll()`.
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
                        if !self.port_is_claimed_by_an_inbound_flow(tcp.dst_port()) {
                            self.ensure_listening(tcp.dst_port());
                        }
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

    fn drive(&mut self) {
        self.ensure_listeners_for_queued_packets();
        // Bridge any newly-accepted host connections BEFORE polling, so their SYN toward the guest
        // goes out on this same cycle rather than waiting a full 5ms tick.
        self.accept_inbound_flows();

        let now = self.now();
        self.iface.poll(now, &mut self.device, &mut self.sockets);

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
                let new_handle = new_listening_tcp_socket(&mut self.sockets, port);
                self.tcp_listeners.insert((port, slot), new_handle);
            }
        }

        self.pump_tcp_flows();
        self.pump_udp();

        // A second poll to flush any smoltcp-side sends queued up by the pumps above.
        let now = self.now();
        self.iface.poll(now, &mut self.device, &mut self.sockets);
        self.diag_gateway_state();
    }

    /// The gateway's own table occupancy, every 256th `drive()`.
    ///
    /// A published port that stops serving while the guest's own loopback connect to it still
    /// answers has to be gateway-side bookkeeping (a flow never reaped, a claim never released, a
    /// socket never removed), which no guest-side per-port diagnostic can see. `claimed_ports`
    /// climbing across ticks is the one that eventually fails every new host connection.
    fn diag_gateway_state(&self) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEEN: AtomicU32 = AtomicU32::new(0);
        if SEEN.fetch_add(1, Ordering::Relaxed) % 256 != 0 {
            return;
        }
        litebox_util_log::warn!(
            flows = self.tcp_flows.len(),
            sockets = self.sockets.iter().count(),
            listeners = self.tcp_listeners.len(),
            claimed_ports = self.ports_claimed_by_inbound_flows.len(),
            inbound_ports = self.inbound_flow_ports.len(),
            udp_flows = self.udp_flows.len();
            "diag-gateway-state: gateway flow and socket occupancy"
        );
    }

    /// Begin proxying a freshly-accepted TCP connection: the socket's local endpoint is the real
    /// destination, since the guest dialed it as if the gateway itself were the destination.
    ///
    /// The `connect()` runs on a short-lived background thread so the gateway's single driver
    /// thread never stalls on a slow or unreachable destination.
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
        let spawn_result = std::thread::Builder::new()
            .name("litebox-tcp-flow-connect".to_owned())
            .spawn(move || {
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
        if let Err(e) = spawn_result {
            eprintln!("[net] failed to spawn tcp-flow-connect thread: {e}");
            return;
        }

        self.tcp_flows.insert(
            handle,
            TcpFlow {
                state: TcpFlowState::Connecting(rx),
                real_eof_or_error: false,
                graceful_fin_sent: false,
                pending_to_real: Vec::new(),
                pending_to_guest: Vec::new(),
            },
        );
    }

    fn pump_tcp_flows(&mut self) {
        let mut to_remove = Vec::new();
        for (&handle, flow) in &mut self.tcp_flows {
            if let TcpFlowState::Connecting(rx) = &flow.state {
                match rx.try_recv() {
                    Ok(Ok(stream)) => flow.state = TcpFlowState::Connected(stream),
                    Ok(Err(_)) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        flow.real_eof_or_error = true;
                        continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => continue,
                }
            }
            let TcpFlowState::Connected(real) = &mut flow.state else {
                unreachable!("just transitioned out of Connecting above, or continued out")
            };

            let socket: &mut tcp::Socket = self.sockets.get_mut(handle);

            if !flow.real_eof_or_error {
                // guest -> real. `real` is nonblocking, so a short or `WouldBlock` write leaves
                // the remainder in `pending_to_real`, retried on the next `drive()` cycle before
                // any further guest data is read, to preserve byte-stream ordering.
                if !flow.pending_to_real.is_empty() {
                    match real.write(&flow.pending_to_real) {
                        Ok(n) => {
                            flow.pending_to_real.drain(..n);
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                        Err(_) => flow.real_eof_or_error = true,
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
                            flow.real_eof_or_error = true;
                            break;
                        }
                    }
                }
                // real -> guest. `send_slice` is a byte-stream partial write too, so its remainder
                // goes to `pending_to_guest` rather than being dropped, which would truncate the
                // guest's byte stream whenever its receive window fell behind.
                if !flow.pending_to_guest.is_empty() && socket.can_send() {
                    match socket.send_slice(&flow.pending_to_guest) {
                        Ok(n) => {
                            flow.pending_to_guest.drain(..n);
                        }
                        Err(_) => flow.real_eof_or_error = true,
                    }
                }
                while flow.pending_to_guest.is_empty() && socket.can_send() {
                    let mut buf = [0u8; 4096];
                    match real.read(&mut buf) {
                        Ok(0) => {
                            flow.real_eof_or_error = true;
                            break;
                        }
                        Ok(n) => match socket.send_slice(&buf[..n]) {
                            Ok(sent) if sent == n => {}
                            Ok(sent) => {
                                flow.pending_to_guest.extend_from_slice(&buf[sent..n]);
                            }
                            Err(_) => {
                                flow.real_eof_or_error = true;
                                break;
                            }
                        },
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(_) => {
                            flow.real_eof_or_error = true;
                            break;
                        }
                    }
                }
            }

            // Close only once every byte read from `real` has reached the guest: a non-empty
            // `pending_to_guest` is still waiting on smoltcp TX space and a non-zero
            // `send_queue()` is still in flight, and closing early truncates the response.
            if flow.real_eof_or_error
                && flow.pending_to_guest.is_empty()
                && socket.send_queue() == 0
            {
                socket.close();
            }
            // The smoltcp-side peer (the guest, for an inbound flow) closed first; shut down only
            // `real`'s write half (see `shutdown_write_not_close_to_avoid_windows_rst` for why not
            // a full close). Gated on the guest->real direction being drained for the same reason
            // the close above is gated on the real->guest one: ending our ability to send while
            // bytes the guest already wrote are still buffered throws the response away -- the
            // ordinary shape of a published request, since a server writes its reply and closes
            // in the same cycle.
            if !socket.is_open()
                && !flow.graceful_fin_sent
                && flow.pending_to_real.is_empty()
                && !socket.can_recv()
            {
                shutdown_write_not_close_to_avoid_windows_rst(real);
                flow.graceful_fin_sent = true;
            }
            let guest_side_done = !socket.is_open() || flow.graceful_fin_sent;
            if guest_side_done
                && flow.real_eof_or_error
                && flow.pending_to_guest.is_empty()
                && socket.send_queue() == 0
            {
                to_remove.push(handle);
            }
        }
        for handle in to_remove {
            self.tcp_flows.remove(&handle);
            if let Some(port) = self.inbound_flow_ports.remove(&handle) {
                self.ports_claimed_by_inbound_flows.remove(&port);
            }
            self.sockets.remove(handle);
        }
    }

    fn ensure_udp_listening(&mut self, dest_port: u16) -> SocketHandle {
        *self
            .udp_listeners
            .entry(dest_port)
            .or_insert_with(|| new_wildcard_udp_socket(&mut self.sockets, dest_port))
    }

    /// Relay each of the guest's datagrams to the real destination through a real `UdpSocket`
    /// keyed by (destination port, guest source port), and relay replies back to the guest
    /// addressed as if they came from the real destination.
    fn pump_udp(&mut self) {
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

/// Windows turns a `close()`/drop of a socket that still holds unread data in its own receive
/// buffer into an abortive RST, so dropping a real socket mid-exchange makes the host peer see
/// "forcibly closed by the remote host" instead of the bytes it was waiting for. Ending only our
/// write half sends a graceful FIN and leaves the drain intact.
fn shutdown_write_not_close_to_avoid_windows_rst(real: &std::net::TcpStream) {
    let _ = real.shutdown(std::net::Shutdown::Write);
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
    // Wildcard *address* (any destination IP), but the destination *port* must match exactly:
    // `smoltcp` has no wildcard-port bind for UDP.
    socket
        .bind(IpListenEndpoint { addr: None, port })
        .expect("bind on a freshly-created UDP socket cannot fail for a nonzero port");
    sockets.add(socket)
}

/// A host-side connection accepted by a published-port listener, waiting to be bridged into the
/// guest. Produced by [`spawn_publish_listener`]'s thread, consumed by
/// [`GatewayState::accept_inbound_flows`].
struct InboundConnection {
    /// The already-accepted, already-nonblocking real socket the host peer is talking to.
    real: std::net::TcpStream,
    /// The port *inside the guest* to bridge to (the right-hand side of `--publish host:guest`).
    guest_port: u16,
}

/// Bind a real Windows listening socket on `127.0.0.1:host_port` and forward every accepted
/// connection to `guest_port` inside the guest: the `docker run -p` / slirp `hostfwd` rule that
/// lets a guest *server* be reached, which outbound NAT alone cannot express.
///
/// Loopback specifically, not `0.0.0.0`: publishing a guest service to the whole network by
/// default would be a surprising exposure decision to make on a user's behalf, and every
/// intended use here (a browser on this machine) is served by loopback.
fn spawn_publish_listener(
    host_port: u16,
    guest_port: u16,
    tx: std::sync::mpsc::Sender<InboundConnection>,
) -> std::io::Result<()> {
    let listener = std::net::TcpListener::bind(SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::LOCALHOST,
        host_port,
    )))?;
    std::thread::Builder::new()
        .name(format!("litebox-publish-{host_port}"))
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(real) = stream else { continue };
                // Nonblocking from the outset: `pump_tcp_flows` polls these by hand on the single
                // gateway thread and must never block on one peer.
                if real.set_nonblocking(true).is_err() {
                    continue;
                }
                // Bound as a local: the log macro takes a `&str` field, not an owned `String`.
                let peer = format_peer(&real);
                litebox_util_log::debug!(
                    host_port = host_port,
                    guest_port = guest_port,
                    peer:% = peer.as_str();
                    "diag-inbound-accept: a host connection on a published port was accepted"
                );
                if tx.send(InboundConnection { real, guest_port }).is_err() {
                    // The gateway thread owns the receiver, so if it is gone this port is over for
                    // the run -- and a host peer then sees it refuse or hang with nothing in the
                    // log to tell that apart from a guest that stopped answering.
                    litebox_util_log::warn!(
                        host_port = host_port,
                        guest_port = guest_port;
                        "diag-inbound-listener: the gateway is gone, this published port stops accepting"
                    );
                    return;
                }
            }
            litebox_util_log::warn!(
                host_port = host_port,
                guest_port = guest_port;
                "diag-inbound-listener: the accept loop ended, this published port stops accepting"
            );
        })?;
    Ok(())
}

/// The peer address of an accepted host socket, or `-` when it cannot be read (a socket closed
/// by its peer between accept and here). Diagnostic-only.
fn format_peer(real: &std::net::TcpStream) -> String {
    match real.peer_addr() {
        Ok(a) => a.to_string(),
        Err(_) => String::from("-"),
    }
}

/// Create a `smoltcp` socket that *connects to* the guest (rather than listening for it), to
/// bridge a host-accepted connection inward. Its local endpoint is the gateway's own IP with an
/// ephemeral port, so the guest sees an ordinary connection arriving from `10.0.0.1`, exactly as
/// it would from a router forwarding a port.
fn new_connecting_tcp_socket(
    sockets: &mut SocketSet<'static>,
    iface: &mut Interface,
    guest_port: u16,
    local_port: u16,
) -> Option<SocketHandle> {
    let mut socket = tcp::Socket::new(
        smoltcp::storage::RingBuffer::new(vec![0u8; SOCKET_BUFFER_SIZE]),
        smoltcp::storage::RingBuffer::new(vec![0u8; SOCKET_BUFFER_SIZE]),
    );
    socket
        .connect(
            iface.context(),
            (IpAddress::Ipv4(GUEST_IP_ADDR), guest_port),
            local_port,
        )
        .ok()?;
    Some(sockets.add(socket))
}

fn new_listening_tcp_socket(sockets: &mut SocketSet<'static>, port: u16) -> SocketHandle {
    let mut socket = tcp::Socket::new(
        smoltcp::storage::RingBuffer::new(vec![0u8; SOCKET_BUFFER_SIZE]),
        smoltcp::storage::RingBuffer::new(vec![0u8; SOCKET_BUFFER_SIZE]),
    );
    // `addr: None` is the wildcard: accept connections whose *destination* is any address, not
    // just our own -- this is what makes the gateway proxy to arbitrary real destinations rather
    // than only accepting connections to `10.0.0.1` itself.
    socket
        .listen(IpListenEndpoint { addr: None, port })
        .expect("listen on a freshly-created socket cannot fail");
    sockets.add(socket)
}

/// Shared handle to the gateway state plus the loopback queue used to exchange packets with the
/// guest-facing `litebox::net::Network` (via [`send_ip_packet`]/[`receive_ip_packet`]).
///
/// Owned by [`crate::WindowsUserland`] as a lazily-initialized field (`OnceLock<NatGateway>`)
/// rather than a module-level `static`: `dev_tests/src/ratchet.rs`'s `ratchet_globals` tracks
/// this crate's bare-static count and is trying to reduce it, so a process-scoped singleton
/// belongs on the one process-lifetime `WindowsUserland` instance instead.
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

        // Published ports (`LITEBOX_PUBLISH=8080:80,3000:3000`), read from the environment rather
        // than plumbed through a CLI argument so every runner gets it without threading a new
        // field through its own arg parsing -- matching how `LITEBOX_DUMP_FRAMES` and friends are
        // already handled.
        let inbound_rx = Self::spawn_published_listeners();

        let gateway_queue = queue.clone();
        let gateway_notify = notify.clone();
        std::thread::Builder::new()
            .name("litebox-nat-gateway".into())
            .spawn(move || {
                let mut state = GatewayState::new(gateway_queue, inbound_rx);
                loop {
                    state.drive();
                    gateway_notify.notify_all();
                    // These are plain `std` sockets polled by hand, with no OS readiness API to
                    // wake us, so the idle sleep is what bounds how soon real-socket readiness is
                    // noticed.
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

    /// Parse `LITEBOX_PUBLISH` and start a host listener per entry.
    ///
    /// Format: `host:guest` pairs, comma-separated (`LITEBOX_PUBLISH=3000:3000,8080:80`). A bare
    /// `port` is shorthand for `port:port`. Returns `None` when unset or when nothing could be
    /// bound, so the gateway skips inbound handling entirely in the common case.
    ///
    /// Every outcome is logged, success included: a published port that silently failed to bind
    /// would present as "the guest's server is broken", sending the next person to debug the
    /// wrong layer.
    fn spawn_published_listeners() -> Option<std::sync::mpsc::Receiver<InboundConnection>> {
        // Same empty-means-unset rule as `init_published_ports`; see its comment.
        let spec = std::env::var("LITEBOX_PUBLISH")
            .ok()
            .filter(|v| !v.trim().is_empty())?;
        let (tx, rx) = std::sync::mpsc::channel();
        let mut bound_any = false;
        for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let (host_str, guest_str) = entry.split_once(':').unwrap_or((entry, entry));
            let (Ok(host_port), Ok(guest_port)) = (
                host_str.trim().parse::<u16>(),
                guest_str.trim().parse::<u16>(),
            ) else {
                litebox_util_log::warn!(
                    "LITEBOX_PUBLISH: ignoring malformed entry {entry:?} (expected `host:guest`, e.g. `3000:3000`)"
                );
                continue;
            };
            match spawn_publish_listener(host_port, guest_port, tx.clone()) {
                Ok(()) => {
                    bound_any = true;
                    litebox_util_log::info!(
                        "published 127.0.0.1:{host_port} -> guest {GUEST_IP_ADDR}:{guest_port}"
                    );
                }
                Err(e) => litebox_util_log::warn!(
                    "LITEBOX_PUBLISH: failed to bind 127.0.0.1:{host_port}: {e}"
                ),
            }
        }
        bound_any.then_some(rx)
    }
}

/// Get (initializing on first use) the [`NatGateway`] behind `slot`.
///
/// Lazy so that non-networked invocations (e.g. `/bin/true`) never pay the cost of spinning up
/// the gateway thread.
fn gateway(slot: &OnceLock<NatGateway>) -> &NatGateway {
    slot.get_or_init(NatGateway::new)
}

/// Start the gateway NOW if any port is published, instead of waiting for the guest to send its
/// first packet.
///
/// [`gateway`] is deliberately lazy, and every *outbound* path forces initialization because the
/// guest sending a packet is what calls it. A **published port has no such trigger**: a guest that
/// only `listen()`s -- an X/VNC bridge, a web UI, exactly the server workloads publishing exists
/// for -- never sends anything, so the gateway would never start, the host listener would never
/// bind, and a browser connecting to `127.0.0.1:<port>` would get connection-refused with nothing
/// in the log to explain it.
///
/// Call once during platform construction. No-op when `LITEBOX_PUBLISH` is unset, preserving the
/// laziness for everyone not publishing a port.
pub(crate) fn init_published_ports(slot: &OnceLock<NatGateway>) {
    // An EMPTY value counts as unset: `process_fork::spawn_process_fork_child` hands a
    // cross-process fork child `LITEBOX_PUBLISH=""` to cancel the parent's inherited value, and
    // omitting the name from the child's environment block means inherit, not remove -- so empty
    // is how the cancellation arrives.
    if std::env::var("LITEBOX_PUBLISH").is_ok_and(|v| !v.trim().is_empty()) {
        let _ = gateway(slot);
    }
}

/// Whether this host process carries the guest's packets (owns the NAT gateway and its published
/// ports). Cross-process fork children do not: their `Network` is the root's shared one, and the
/// gateway's queue exists only in the root process (see
/// [`litebox::platform::IPInterfaceProvider::owns_ip_interface`]).
pub(crate) fn owns_ip_interface() -> bool {
    static OWNS: OnceLock<bool> = OnceLock::new();
    *OWNS.get_or_init(|| std::env::var_os(crate::process_fork::FORK_CHILD_GPRS_ENV_VAR).is_none())
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
/// One line per N packets on the guest<->gateway wire.
///
/// chrF11 measured selkies' 8081 refusing every in-guest connect from t=30s while an idle 8082
/// beside it answered 200 at the same instant: same queue, same poller, same netstack, same
/// process issuing both connects -- only the port differs. Every per-port state the socket layer
/// can print looked healthy for 8081 (`slots=8 listening=8 pending=0`, and `diag-tick` reached it
/// at uptime 226s), so the one question left is whether 8081's SYN reaches the stack at all --
/// which only the wire itself can answer. `rx` without a matching `tx` is a stack that dropped or
/// refused it; `tx` with no `rx` is a queue nobody drained.
fn diag_pkt(dir: &'static str, packet: &[u8]) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEEN: AtomicU32 = AtomicU32::new(0);
    if SEEN.fetch_add(1, Ordering::Relaxed) % 512 != 0 {
        return;
    }
    let Ok(ip) = Ipv4Packet::new_checked(packet) else {
        return;
    };
    let src = ip.src_addr();
    let dst = ip.dst_addr();
    let tuple = match ip.next_header() {
        IpProtocol::Tcp => match TcpPacket::new_checked(ip.payload()) {
            Ok(t) => format!(
                "tcp {}:{} -> {}:{} {}{}{}{}",
                src,
                t.src_port(),
                dst,
                t.dst_port(),
                if t.syn() { "S" } else { "" },
                if t.ack() { "A" } else { "" },
                if t.rst() { "R" } else { "" },
                if t.fin() { "F" } else { "" },
            ),
            Err(_) => format!("tcp {src} -> {dst} unparsed"),
        },
        IpProtocol::Udp => match UdpPacket::new_checked(ip.payload()) {
            Ok(u) => format!("udp {}:{} -> {}:{}", src, u.src_port(), dst, u.dst_port()),
            Err(_) => format!("udp {src} -> {dst} unparsed"),
        },
        _ => format!("other {src} -> {dst}"),
    };
    litebox_util_log::warn!(
        dir:% = dir,
        pid = std::process::id(),
        tuple:% = tuple;
        "diag-pkt: packet on the guest<->gateway wire"
    );
}

pub(crate) fn send_ip_packet(
    slot: &OnceLock<NatGateway>,
    packet: &[u8],
) -> Result<(), litebox::platform::SendError> {
    diag_pkt("tx", packet);
    let gw = gateway(slot);
    // A guest packet to 127.0.0.0/8 or to GUEST_IP_ADDR never reaches this hook: `phy::TxToken::
    // consume` diverts it into the guest's own in-process loopback queue (`is_local_ipv4`, the
    // predicate `packet_targets_guest_loopback` recomputed here) and never calls the platform. So
    // the arm that used to live here -- pushing into `to_guest`, the RECEIVE queue, from the SEND
    // hook -- was dead, and its `diag_loop("sent", ..)` could never fire: chrF21 logged 68
    // refusals on 8081 and not one line from it. Guest-to-guest traffic is instrumented in
    // `litebox/src/net/phy.rs`, on the queue it actually crosses.
    let mut queue = gw.queue.lock().unwrap();
    queue.to_gateway.push_back(packet.to_vec());
    Ok(())
}

/// Every TCP handshake packet the GATEWAY hands the guest, unthrottled at `debug`.
///
/// `diag_pkt` samples 1/512 of the wire, too coarse for "did this SYN reach the stack at all".
/// What this sees is everything arriving from OUTSIDE the guest: an inbound published-port flow's
/// SYN, the RST or SA answering it, and replies to guest-initiated connections. A guest's OWN
/// packet to 127.0.0.1 or GUEST_IP_ADDR never reaches this path: `phy::TxToken::consume` loops it
/// in-process and logs it under `diag-loop` there. Renamed from `diag-loop`: sharing a name mixed a
/// guest-to-guest queue with a gateway-to-guest one and no line said which path it came from.
fn diag_loop_inbound(dir: &'static str, packet: &[u8]) {
    let Ok(ip) = Ipv4Packet::new_checked(packet) else {
        return;
    };
    let Ok(t) = TcpPacket::new_checked(ip.payload()) else {
        return;
    };
    if !(t.syn() || t.rst() || t.fin()) {
        return;
    }
    litebox_util_log::debug!(
        dir:% = dir,
        pid = std::process::id(),
        src:% = format!("{}:{}", ip.src_addr(), t.src_port()),
        dst:% = format!("{}:{}", ip.dst_addr(), t.dst_port()),
        flags:% = format!(
            "{}{}{}{}",
            if t.syn() { "S" } else { "" },
            if t.ack() { "A" } else { "" },
            if t.rst() { "R" } else { "" },
            if t.fin() { "F" } else { "" }
        );
        "diag-loop-inbound: TCP handshake packet from the gateway to the guest"
    );
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
    diag_pkt("rx", &packet[..n]);
    diag_loop_inbound("delivered", &packet[..n]);
    Ok(n)
}

/// Block the calling thread until either a packet is available to read from the gateway, or
/// `timeout` elapses. Mirrors `LinuxUserland::wait_on_tun`'s role for the network-worker thread.
pub(crate) fn wait_on_tun(slot: &OnceLock<NatGateway>, timeout: Option<core::time::Duration>) {
    if !owns_ip_interface() {
        // No gateway in this process to wait on, and none may be started: a second one would
        // fight the owner for the published ports.
        std::thread::sleep(timeout.unwrap_or(Duration::from_millis(50)));
        return;
    }
    let gw = gateway(slot);
    let has_packet = || !gw.queue.lock().unwrap().to_guest.is_empty();
    if has_packet() {
        return;
    }
    let guard = gw.notify_lock.lock().unwrap();
    let timeout = timeout.unwrap_or(Duration::from_millis(50));
    let _ = gw.notify.wait_timeout(guard, timeout);
}
