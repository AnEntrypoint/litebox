// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Connection to the physical (i.e., "lower") side for networking.

// TODO(jayb): Do we need to wrap/unwrap the IPv4 header here, or is a better place within the
// implementer of the `platform::IPInterfaceProvider` trait?

use crate::platform;

/// The maximum transmission unit for a device
pub(crate) const DEVICE_MTU: usize = 1600;

pub(crate) struct Device<Platform: platform::IPInterfaceProvider + 'static> {
    pub(crate) platform: &'static Platform,
    receive_buffer: [u8; DEVICE_MTU],
    send_buffer: [u8; DEVICE_MTU],
    /// Packets the guest addressed to itself (`127.0.0.0/8` or the interface's own address).
    /// smoltcp only ever hands a packet to the device; delivering it back to a local socket is the
    /// device's job, exactly as a kernel's `lo` does.
    pub(crate) loopback: alloc::collections::VecDeque<alloc::vec::Vec<u8>>,
}

impl<Platform: platform::IPInterfaceProvider> Device<Platform> {
    pub(crate) fn new(platform: &'static Platform) -> Self {
        Self {
            platform,
            receive_buffer: [0u8; DEVICE_MTU],
            send_buffer: [0u8; DEVICE_MTU],
            loopback: alloc::collections::VecDeque::new(),
        }
    }
}

impl<Platform: platform::IPInterfaceProvider> smoltcp::phy::Device for Device<Platform> {
    type RxToken<'a>
        = RxToken<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken<'a, Platform>
    where
        Self: 'a;

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let received = if let Some(packet) = self.loopback.pop_front() {
            let size = packet.len().min(DEVICE_MTU);
            diag_loop("delivered", &packet[..size], self.loopback.len());
            self.receive_buffer[..size].copy_from_slice(&packet[..size]);
            Some(size)
        } else {
            match self.platform.receive_ip_packet(&mut self.receive_buffer) {
                Ok(size) => Some(size),
                Err(platform::ReceiveError::WouldBlock) => None,
            }
        };
        received.map(|size| {
            (
                RxToken {
                    buffer: &self.receive_buffer[..size],
                },
                TxToken {
                    platform: self.platform,
                    buffer: &mut self.send_buffer,
                    loopback: &mut self.loopback,
                },
            )
        })
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        Some(TxToken {
            platform: self.platform,
            buffer: &mut self.send_buffer,
            loopback: &mut self.loopback,
        })
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = smoltcp::phy::DeviceCapabilities::default();
        caps.medium = smoltcp::phy::Medium::Ip;
        caps.max_transmission_unit = DEVICE_MTU;
        caps
    }
}

pub(crate) struct RxToken<'a> {
    buffer: &'a [u8],
}

impl smoltcp::phy::RxToken for RxToken<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.buffer)
    }
}

pub(crate) struct TxToken<'a, Platform: platform::IPInterfaceProvider> {
    platform: &'a Platform,
    buffer: &'a mut [u8],
    loopback: &'a mut alloc::collections::VecDeque<alloc::vec::Vec<u8>>,
}

/// Whether `packet` is an IPv4 packet addressed to this host itself.
fn is_local_ipv4(packet: &[u8]) -> bool {
    packet.len() >= 20
        && packet[0] >> 4 == 4
        && (packet[16] == 127 || packet[16..20] == super::INTERFACE_IP_ADDR.octets())
}

/// One `diag-loop` line for a guest-to-guest packet, at whichever end of [`Device::loopback`] it
/// passes: `sent` is the push in [`TxToken::consume`], `delivered` the pop in [`Device::receive`].
///
/// [`Device::loopback`] is the ONLY path such a packet takes and `platform`'s send/receive hooks
/// never see it, so an instrument placed there is blind to exactly the traffic a probe generates
/// when it dials a port in its own guest: chrF21 logged 68 connect refusals on 8081 and not one
/// `diag-loop` line, because every packet that decided them went through this queue. Seeing both
/// ends is what separates "the SYN was never taken off the queue" from "the SYN was delivered and
/// answered with an RST", which no amount of port bookkeeping can distinguish.
///
/// Only SYN/FIN/RST are logged as they pass; the rest are counted and sampled 1-in-512, because a
/// stream's own data packets would otherwise be the whole log.
fn diag_loop(dir: &'static str, packet: &[u8], depth: usize) {
    if packet.len() < 20 || packet[0] >> 4 != 4 || packet[9] != 6 {
        return;
    }
    let ihl = ((packet[0] & 0xf) as usize) * 4;
    if packet.len() < ihl + 20 {
        return;
    }
    let tcp = &packet[ihl..];
    let flags = tcp[13];
    if flags & 0x07 == 0 {
        static OTHER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        if OTHER.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % 512 != 0 {
            return;
        }
    }
    let seq = u32::from_be_bytes([tcp[4], tcp[5], tcp[6], tcp[7]]);
    let ack = u32::from_be_bytes([tcp[8], tcp[9], tcp[10], tcp[11]]);
    litebox_util_log::debug!(
        tag = super::host_process_tag(), dir:% = dir, depth = depth,
        src = alloc::format!(
            "{}.{}.{}.{}:{}", packet[12], packet[13], packet[14], packet[15],
            u16::from_be_bytes([tcp[0], tcp[1]])
        ),
        dst = alloc::format!(
            "{}.{}.{}.{}:{}", packet[16], packet[17], packet[18], packet[19],
            u16::from_be_bytes([tcp[2], tcp[3]])
        ),
        flags = flags, seq = seq, ack = ack;
        "diag-loop: a guest-to-guest TCP packet crossed the loopback queue"
    );
}

impl<Platform: platform::IPInterfaceProvider> smoltcp::phy::TxToken for TxToken<'_, Platform> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let packet = &mut self.buffer[..len];
        let res = f(packet);
        if is_local_ipv4(packet) {
            self.loopback.push_back(packet.to_vec());
            diag_loop("sent", packet, self.loopback.len());
        } else {
            self.platform
                .send_ip_packet(packet)
                .expect("Sending IP packet failed");
        }
        res
    }
}
