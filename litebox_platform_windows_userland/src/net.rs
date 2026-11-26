#![allow(clippy::pedantic)]

use std::sync::OnceLock;
use std::time::Duration;

pub use litebox_userspace_net::NatGateway;

pub fn init_published_ports(slot: &OnceLock<NatGateway>) {
    litebox_userspace_net::init_published_ports(slot);
}

pub fn owns_ip_interface() -> bool {
    static OWNS: OnceLock<bool> = OnceLock::new();
    *OWNS.get_or_init(|| std::env::var_os(crate::process_fork::FORK_CHILD_GPRS_ENV_VAR).is_none())
}

pub fn send_ip_packet(
    slot: &OnceLock<NatGateway>,
    packet: &[u8],
) -> Result<(), litebox::platform::SendError> {
    litebox_userspace_net::send_ip_packet(slot, packet)
}

pub fn receive_ip_packet(
    slot: &OnceLock<NatGateway>,
    packet: &mut [u8],
) -> Result<usize, litebox::platform::ReceiveError> {
    litebox_userspace_net::receive_ip_packet(slot, packet)
}

pub fn wait_on_tun(slot: &OnceLock<NatGateway>, timeout: Option<Duration>) {
    litebox_userspace_net::wait_on_tun(slot, timeout, owns_ip_interface());
}
