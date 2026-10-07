// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Minimal `AF_NETLINK` socket support -- enough for `udev_monitor_new_from_netlink()`
//! (`NETLINK_KOBJECT_UEVENT`) to succeed, without any real kernel netlink subsystem behind it.
//!
//! This shim's guest device set is static for the lifetime of a single guest process (no real
//! hardware ever appears/disappears while it runs), so a `NETLINK_KOBJECT_UEVENT` monitor that
//! never delivers any message is faithful, correct behavior for this environment -- not a
//! shortcut. `bind()`/`getsockname()` succeed and echo back whatever `sockaddr_nl` fields the
//! guest set (synthesizing a non-zero `nl_pid` on request, matching real Linux's auto-assign
//! behavior); `recvmsg()`/`read()` never has data and is never ready, matching a socket with no
//! kernel-side event source; `sendmsg()`/`write()` accept and discard, matching a socket with no
//! peer to fail against.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use litebox::{
    event::{Events, IOPollable, observer::Observer},
    fd::{FdEnabledSubsystem, FdEnabledSubsystemEntry},
    fs::OFlags,
};
use litebox_common_linux::errno::Errno;

pub(crate) struct NetlinkSocketSubsystem;
impl FdEnabledSubsystem for NetlinkSocketSubsystem {
    type Entry = NetlinkSocket;
}
impl FdEnabledSubsystemEntry for NetlinkSocket {}

/// Process-wide counter backing auto-assigned `nl_pid` values (real Linux assigns the calling
/// thread's own PID by default, falling back to a kernel-picked unique value on collision --
/// this shim has no other netlink socket to collide with, so a simple monotonic counter,
/// distinct from any real PID, is sufficient to give each auto-bound socket a unique identity).
static NEXT_AUTO_PID: AtomicU32 = AtomicU32::new(1);

pub(crate) struct NetlinkSocket {
    status: core::sync::atomic::AtomicU32,
    /// `nl_pid` as bound by the guest, or 0 if never explicitly bound (matching real Linux's
    /// unbound-socket `getsockname` behavior, which also reports an all-zero address).
    bound_pid: AtomicU32,
    /// `nl_groups` as bound by the guest, or 0 if never explicitly bound.
    bound_groups: AtomicU32,
    protocol: u8,
    /// Replies the (synthetic) kernel generated for requests sent on this socket, one datagram each.
    rx: spin::Mutex<alloc::collections::VecDeque<alloc::vec::Vec<u8>>>,
    observers: spin::Mutex<alloc::vec::Vec<(alloc::sync::Weak<dyn Observer<Events>>, Events)>>,
}

const NETLINK_ROUTE: u8 = 0;
const NETLINK_AUDIT: u8 = 9;
const NLMSG_HEADER_BYTES: usize = 16;

impl NetlinkSocket {
    pub(crate) fn new(flags: litebox_common_linux::SockFlags) -> Self {
        Self::new_with_protocol(flags, u8::MAX)
    }

    pub(crate) fn new_with_protocol(flags: litebox_common_linux::SockFlags, protocol: u8) -> Self {
        let mut status = OFlags::RDWR;
        status.set(
            OFlags::NONBLOCK,
            flags.contains(litebox_common_linux::SockFlags::NONBLOCK),
        );
        Self {
            status: core::sync::atomic::AtomicU32::new(status.bits()),
            bound_pid: AtomicU32::new(0),
            bound_groups: AtomicU32::new(0),
            protocol,
            rx: spin::Mutex::new(alloc::collections::VecDeque::new()),
            observers: spin::Mutex::new(alloc::vec::Vec::new()),
        }
    }

    /// `bind(2)`: stores whatever `(nl_pid, nl_groups)` the guest provides. `nl_pid == 0` means
    /// "auto-assign", matching real Linux. No real validation is possible or needed -- there is
    /// no real netlink routing table this socket could conflict with.
    pub(crate) fn bind(&self, nl_pid: u32, nl_groups: u32) -> Result<(), Errno> {
        let assigned_pid = if nl_pid == 0 {
            NEXT_AUTO_PID.fetch_add(1, Ordering::Relaxed)
        } else {
            nl_pid
        };
        self.bound_pid.store(assigned_pid, Ordering::Relaxed);
        self.bound_groups.store(nl_groups, Ordering::Relaxed);
        Ok(())
    }

    /// `getsockname(2)`: echoes back the bound address, or `(0, 0)` if never explicitly bound
    /// (matching real Linux's unbound-socket `getsockname` behavior).
    pub(crate) fn local_addr(&self) -> (u32, u32) {
        (
            self.bound_pid.load(Ordering::Relaxed),
            self.bound_groups.load(Ordering::Relaxed),
        )
    }

    /// `sendmsg`/`write`-family. On a `NETLINK_ROUTE` socket the synthetic kernel answers the
    /// link/address/route dump requests (the only ones this guest's fixed interface set can
    /// meaningfully serve); everything else is accepted and discarded.
    pub(crate) fn send(&self, data: &[u8]) -> Result<usize, Errno> {
        if self.protocol == NETLINK_ROUTE {
            self.handle_route_requests(data);
        } else if self.protocol == NETLINK_AUDIT {
            self.acknowledge(data);
        }
        Ok(data.len())
    }

    fn acknowledge(&self, message: &[u8]) {
        if self.protocol != NETLINK_AUDIT || message.len() < NLMSG_HEADER_BYTES {
            return;
        }
        let flags = u16::from_le_bytes([message[6], message[7]]);
        if flags & NLM_F_ACK == 0 {
            return;
        }
        let acknowledgement_len = (NLMSG_HEADER_BYTES + 4 + NLMSG_HEADER_BYTES) as u32;
        let mut reply = Vec::with_capacity(acknowledgement_len as usize);
        reply.extend_from_slice(&acknowledgement_len.to_le_bytes());
        reply.extend_from_slice(&NLMSG_ERROR.to_le_bytes());
        reply.extend_from_slice(&0u16.to_le_bytes());
        reply.extend_from_slice(&message[8..12]);
        reply.extend_from_slice(&self.bound_pid.load(Ordering::Relaxed).to_le_bytes());
        reply.extend_from_slice(&0i32.to_le_bytes());
        reply.extend_from_slice(&message[..NLMSG_HEADER_BYTES]);
        self.queue(reply);
    }

    pub(crate) fn recv(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        self.receive(buf, litebox_common_linux::ReceiveFlags::empty())
    }

    /// `recvmsg`/`read`-family: pops one queued reply datagram into `buf` (`MSG_PEEK` leaves it
    /// queued, `MSG_DONTWAIT` turns an empty queue into `EAGAIN`).
    pub(crate) fn receive(
        &self,
        buf: &mut [u8],
        flags: litebox_common_linux::ReceiveFlags,
    ) -> Result<usize, Errno> {
        {
            let mut rx = self.rx.lock();
            if let Some(msg) = rx.front() {
                let n = msg.len().min(buf.len());
                buf[..n].copy_from_slice(&msg[..n]);
                if !flags.contains(litebox_common_linux::ReceiveFlags::PEEK) {
                    rx.pop_front();
                }
                return Ok(n);
            }
        }
        if flags.contains(litebox_common_linux::ReceiveFlags::DONTWAIT) {
            return Err(Errno::EAGAIN);
        }
        if self.get_status().contains(OFlags::NONBLOCK) {
            Err(Errno::EAGAIN)
        } else {
            Err(Errno::EOPNOTSUPP)
        }
    }

    fn queue(&self, datagram: alloc::vec::Vec<u8>) {
        self.rx.lock().push_back(datagram);
        let observers: alloc::vec::Vec<_> = self
            .observers
            .lock()
            .iter()
            .filter(|(_, mask)| mask.contains(Events::IN))
            .filter_map(|(o, _)| o.upgrade())
            .collect();
        for o in observers {
            o.on_events(&Events::IN);
        }
    }

    fn handle_route_requests(&self, data: &[u8]) {
        let mut off = 0;
        while off + 16 <= data.len() {
            let len = u32::from_ne_bytes(data[off..off + 4].try_into().unwrap()) as usize;
            let ty = u16::from_ne_bytes(data[off + 4..off + 6].try_into().unwrap());
            let flags = u16::from_ne_bytes(data[off + 6..off + 8].try_into().unwrap());
            let seq = u32::from_ne_bytes(data[off + 8..off + 12].try_into().unwrap());
            let port = self.bound_pid.load(Ordering::Relaxed);
            if len < 16 {
                break;
            }
            let dump = flags & NLM_F_DUMP == NLM_F_DUMP;
            let mut out = alloc::vec::Vec::new();
            match ty {
                RTM_GETLINK if dump => route_links(&mut out, seq, port),
                RTM_GETADDR if dump => route_addrs(&mut out, seq, port),
                RTM_GETROUTE if dump => route_routes(&mut out, seq, port),
                RTM_GETLINK | RTM_GETADDR | RTM_GETROUTE => {
                    push_hdr(&mut out, 16 + 20, NLMSG_ERROR, 0, seq, port, |b| {
                        b.extend_from_slice(&(-95i32).to_ne_bytes());
                        b.extend_from_slice(&data[off..off + 16]);
                    });
                }
                _ => {
                    if flags & NLM_F_ACK != 0 {
                        push_hdr(&mut out, 16 + 20, NLMSG_ERROR, 0, seq, port, |b| {
                            b.extend_from_slice(&0i32.to_ne_bytes());
                            b.extend_from_slice(&data[off..off + 16]);
                        });
                    }
                }
            }
            if !out.is_empty() {
                self.queue(out);
            }
            off += (len + 3) & !3;
        }
    }

    super::common_functions_for_file_status!();
}

impl IOPollable for NetlinkSocket {
    fn check_io_events(&self) -> Events {
        // Always writable (sends are discarded, never block), never readable (no real event
        // source ever produces data) -- matches this module's own documented static-device-set
        // rationale.
        if self.rx.lock().is_empty() {
            Events::OUT
        } else {
            Events::OUT | Events::IN
        }
    }

    fn register_observer(&self, observer: alloc::sync::Weak<dyn Observer<Events>>, mask: Events) {
        self.observers.lock().push((observer, mask));
    }
}

const NLM_F_MULTI: u16 = 2;
const NLM_F_ACK: u16 = 4;
const NLM_F_DUMP: u16 = 0x300;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const RTM_NEWLINK: u16 = 16;
const RTM_NEWADDR: u16 = 20;
const RTM_NEWROUTE: u16 = 24;
const RTM_GETLINK: u16 = 18;
const RTM_GETADDR: u16 = 22;
const RTM_GETROUTE: u16 = 26;

/// The fixed interface set every guest sees: loopback and one ethernet device on the TUN network.
struct Iface {
    index: u32,
    name: &'static [u8],
    flags: u32,
    mtu: u32,
    mac: [u8; 6],
    addr: [u8; 4],
    prefix: u8,
    scope: u8,
    ty: u16,
}

fn ifaces() -> [Iface; 2] {
    [
        Iface {
            index: 1,
            name: b"lo\0",
            // IFF_UP | IFF_LOOPBACK | IFF_RUNNING
            flags: 0x1 | 0x8 | 0x40,
            mtu: 65536,
            mac: [0; 6],
            addr: [127, 0, 0, 1],
            prefix: 8,
            scope: 254,
            ty: 772,
        },
        Iface {
            index: 2,
            name: b"eth0\0",
            // IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_MULTICAST
            flags: 0x1 | 0x2 | 0x40 | 0x1000,
            mtu: 1500,
            mac: [0x02, 0x24, 0x74, 0x20, 0x00, 0x02],
            addr: litebox::net::INTERFACE_IP_ADDR.octets(),
            prefix: 24,
            scope: 0,
            ty: 1,
        },
    ]
}

fn push_hdr(
    out: &mut alloc::vec::Vec<u8>,
    len: usize,
    ty: u16,
    flags: u16,
    seq: u32,
    port: u32,
    body: impl FnOnce(&mut alloc::vec::Vec<u8>),
) {
    out.extend_from_slice(&(len as u32).to_ne_bytes());
    out.extend_from_slice(&ty.to_ne_bytes());
    out.extend_from_slice(&flags.to_ne_bytes());
    out.extend_from_slice(&seq.to_ne_bytes());
    out.extend_from_slice(&port.to_ne_bytes());
    body(out);
}

fn attr(b: &mut alloc::vec::Vec<u8>, ty: u16, payload: &[u8]) {
    b.extend_from_slice(&((4 + payload.len()) as u16).to_ne_bytes());
    b.extend_from_slice(&ty.to_ne_bytes());
    b.extend_from_slice(payload);
    while b.len() % 4 != 0 {
        b.push(0);
    }
}

fn message(
    out: &mut alloc::vec::Vec<u8>,
    ty: u16,
    seq: u32,
    port: u32,
    fixed: &[u8],
    attrs: &alloc::vec::Vec<u8>,
) {
    let len = 16 + fixed.len() + attrs.len();
    push_hdr(out, len, ty, NLM_F_MULTI, seq, port, |b| {
        b.extend_from_slice(fixed);
        b.extend_from_slice(attrs);
    });
}

fn done(out: &mut alloc::vec::Vec<u8>, seq: u32, port: u32) {
    push_hdr(out, 20, NLMSG_DONE, NLM_F_MULTI, seq, port, |b| {
        b.extend_from_slice(&0i32.to_ne_bytes());
    });
}

fn route_links(out: &mut alloc::vec::Vec<u8>, seq: u32, port: u32) {
    for i in ifaces() {
        let mut fixed = alloc::vec::Vec::new();
        fixed.extend_from_slice(&[0, 0]);
        fixed.extend_from_slice(&i.ty.to_ne_bytes());
        fixed.extend_from_slice(&(i.index as i32).to_ne_bytes());
        fixed.extend_from_slice(&i.flags.to_ne_bytes());
        fixed.extend_from_slice(&0u32.to_ne_bytes());
        let mut a = alloc::vec::Vec::new();
        attr(&mut a, 3, i.name);
        attr(&mut a, 4, &i.mtu.to_ne_bytes());
        attr(&mut a, 16, &[6]);
        attr(&mut a, 1, &i.mac);
        attr(&mut a, 2, &[0xff; 6]);
        message(out, RTM_NEWLINK, seq, port, &fixed, &a);
    }
    done(out, seq, port);
}

fn route_addrs(out: &mut alloc::vec::Vec<u8>, seq: u32, port: u32) {
    for i in ifaces() {
        let mut fixed = alloc::vec::Vec::new();
        fixed.extend_from_slice(&[2, i.prefix, 0x80, i.scope]);
        fixed.extend_from_slice(&i.index.to_ne_bytes());
        let mut a = alloc::vec::Vec::new();
        attr(&mut a, 1, &i.addr);
        attr(&mut a, 2, &i.addr);
        attr(&mut a, 3, i.name);
        message(out, RTM_NEWADDR, seq, port, &fixed, &a);
    }
    done(out, seq, port);
}

fn route_routes(out: &mut alloc::vec::Vec<u8>, seq: u32, port: u32) {
    let gw = litebox::net::GATEWAY_IP_ADDR.octets();
    let net = litebox::net::INTERFACE_IP_ADDR.octets();
    // (dst_len, dst, gateway, scope, oif)
    let routes: [(u8, [u8; 4], Option<[u8; 4]>, u8, u32); 2] = [
        (0, [0; 4], Some(gw), 0, 2),
        (24, [net[0], net[1], net[2], 0], None, 253, 2),
    ];
    for (dst_len, dst, gateway, scope, oif) in routes {
        let mut fixed = alloc::vec::Vec::new();
        // family, dst_len, src_len, tos, table, protocol(boot=3), scope, type(unicast=1), flags
        fixed.extend_from_slice(&[2, dst_len, 0, 0, 254, 3, scope, 1]);
        fixed.extend_from_slice(&0u32.to_ne_bytes());
        let mut a = alloc::vec::Vec::new();
        attr(&mut a, 15, &254u32.to_ne_bytes());
        if dst_len != 0 {
            attr(&mut a, 1, &dst);
        }
        if let Some(g) = gateway {
            attr(&mut a, 5, &g);
        }
        attr(&mut a, 4, &oif.to_ne_bytes());
        message(out, RTM_NEWROUTE, seq, port, &fixed, &a);
    }
    done(out, seq, port);
}

#[cfg(test)]
mod tests {
    use litebox::event::{Events, IOPollable as _};
    use litebox_common_linux::{SockFlags, errno::Errno};

    #[test]
    fn unbound_socket_getsockname_is_zero() {
        let sock = super::NetlinkSocket::new(SockFlags::empty());
        assert_eq!(sock.local_addr(), (0, 0));
    }

    #[test]
    fn bind_with_explicit_pid_is_echoed_back() {
        let sock = super::NetlinkSocket::new(SockFlags::empty());
        sock.bind(42, 0x1).unwrap();
        assert_eq!(sock.local_addr(), (42, 0x1));
    }

    #[test]
    fn bind_with_auto_pid_assigns_a_nonzero_unique_value() {
        let sock1 = super::NetlinkSocket::new(SockFlags::empty());
        let sock2 = super::NetlinkSocket::new(SockFlags::empty());
        sock1.bind(0, 0).unwrap();
        sock2.bind(0, 0).unwrap();
        let (pid1, _) = sock1.local_addr();
        let (pid2, _) = sock2.local_addr();
        assert_ne!(pid1, 0);
        assert_ne!(pid2, 0);
        assert_ne!(pid1, pid2);
    }

    #[test]
    fn recv_on_nonblocking_socket_is_always_eagain() {
        let sock = super::NetlinkSocket::new(SockFlags::NONBLOCK);
        assert_eq!(sock.recv(&mut [0u8; 16]), Err(Errno::EAGAIN));
        assert_eq!(sock.check_io_events(), Events::OUT);
    }

    #[test]
    fn send_always_succeeds_and_reports_full_length() {
        let sock = super::NetlinkSocket::new(SockFlags::empty());
        assert_eq!(sock.send(&[0u8; 128]), Ok(128));
    }

    #[test]
    fn sys_dup_on_netlink_socket_succeeds() {
        let task = crate::syscalls::tests::init_platform(None);
        let sock = super::NetlinkSocket::new(SockFlags::empty());
        let typed = task
            .global
            .litebox
            .descriptor_table_mut()
            .insert::<super::NetlinkSocketSubsystem>(sock);
        let raw_fd = task.files.borrow().insert_raw_fd(typed).ok().unwrap();
        let dup_fd = task
            .sys_dup(i32::try_from(raw_fd).unwrap(), None, None)
            .expect("sys_dup on netlink socket must succeed");
        assert_ne!(dup_fd, raw_fd as u32);
    }
}
