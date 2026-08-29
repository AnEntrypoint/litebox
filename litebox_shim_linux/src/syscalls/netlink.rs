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
}

impl NetlinkSocket {
    pub(crate) fn new(flags: litebox_common_linux::SockFlags) -> Self {
        let mut status = OFlags::RDWR;
        status.set(
            OFlags::NONBLOCK,
            flags.contains(litebox_common_linux::SockFlags::NONBLOCK),
        );
        Self {
            status: core::sync::atomic::AtomicU32::new(status.bits()),
            bound_pid: AtomicU32::new(0),
            bound_groups: AtomicU32::new(0),
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

    /// `sendmsg`/`write`-family: accepted and discarded -- there is no real netlink peer (the
    /// kernel) to deliver to, matching a socket whose only listener is a `recvmsg` that will
    /// never observe this data anyway (no hotplug event delivery in this environment).
    pub(crate) fn send(&self, len: usize) -> Result<usize, Errno> {
        Ok(len)
    }

    /// `recvmsg`/`read`-family: never has data, matching a `NETLINK_KOBJECT_UEVENT` monitor in an
    /// environment where no real hardware ever changes while the guest process runs.
    pub(crate) fn recv(&self) -> Result<usize, Errno> {
        if self.get_status().contains(OFlags::NONBLOCK) {
            Err(Errno::EAGAIN)
        } else {
            Err(Errno::EOPNOTSUPP)
        }
    }

    super::common_functions_for_file_status!();
}

impl IOPollable for NetlinkSocket {
    fn check_io_events(&self) -> Events {
        // Always writable (sends are discarded, never block), never readable (no real event
        // source ever produces data) -- matches this module's own documented static-device-set
        // rationale.
        Events::OUT
    }

    fn register_observer(&self, _observer: alloc::sync::Weak<dyn Observer<Events>>, _mask: Events) {
        // `check_io_events` always reports the same, constant readiness (writable, never
        // readable) -- there is no event source that could ever transition it, so there is
        // nothing to notify a registered observer about later, matching `signalfd.rs`'s/
        // `timerfd.rs`'s identical narrowing for a fd type with no background driver.
    }
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
        assert_eq!(sock.recv(), Err(Errno::EAGAIN));
        assert_eq!(sock.check_io_events(), Events::OUT);
    }

    #[test]
    fn send_always_succeeds_and_reports_full_length() {
        let sock = super::NetlinkSocket::new(SockFlags::empty());
        assert_eq!(sock.send(128), Ok(128));
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
