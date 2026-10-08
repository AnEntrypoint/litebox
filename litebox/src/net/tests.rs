// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use platform::mock::MockPlatform;

use super::*;

use core::net::SocketAddrV4;
use core::str::FromStr;

extern crate std;

fn bidi_tcp_comms(mut network: Network<MockPlatform>, comms: fn(&mut Network<MockPlatform>)) {
    // Create a listening socket
    let listener_fd = network
        .socket(Protocol::Tcp)
        .expect("Failed to create TCP socket");
    let listen_addr = SocketAddr::V4(SocketAddrV4::from_str("10.0.0.2:8080").unwrap());

    network
        .bind(&listener_fd, &listen_addr)
        .expect("Failed to bind TCP socket");
    network
        .listen(&listener_fd, 1)
        .expect("Failed to listen on TCP socket");

    // Create a connecting socket
    let client_fd = network
        .socket(Protocol::Tcp)
        .expect("Failed to create TCP socket");
    let err = network
        .connect(&client_fd, &listen_addr, false)
        .unwrap_err();
    assert!(
        matches!(err, ConnectError::InProgress),
        "Expected InProgress error, got {err:?}",
    );

    comms(&mut network);

    // Accept the connection on the listening socket
    let server_fd = loop {
        match network.accept(&listener_fd, None) {
            Ok(fd) => break fd,
            Err(AcceptError::NoConnectionsReady) => {}
            Err(other) => panic!("Unexpected accept error: {other:?}"),
        }
    };

    // Send data from client to server
    let client_to_server_data = b"Hello from client!";
    let bytes_sent = network
        .send(&client_fd, client_to_server_data, SendFlags::empty(), None)
        .expect("Failed to send data");
    assert_eq!(bytes_sent, client_to_server_data.len());

    comms(&mut network);

    // Receive data on the server
    let mut server_buffer = [0u8; 1024];
    let bytes_received = network
        .receive(&server_fd, &mut server_buffer, ReceiveFlags::empty(), None)
        .expect("Failed to receive data");
    assert_eq!(&server_buffer[..bytes_received], client_to_server_data);

    // Send data from server to client
    let server_to_client_data = b"Hello from server!";
    let bytes_sent = network
        .send(&server_fd, server_to_client_data, SendFlags::empty(), None)
        .expect("Failed to send data");
    assert_eq!(bytes_sent, server_to_client_data.len());

    comms(&mut network);

    // Receive data on the client
    let mut client_buffer = [0u8; 1024];
    let bytes_received = network
        .receive(&client_fd, &mut client_buffer, ReceiveFlags::empty(), None)
        .expect("Failed to receive data");
    assert_eq!(&client_buffer[..bytes_received], server_to_client_data);

    network.close(&client_fd, CloseBehavior::Immediate).unwrap();
    network.close(&server_fd, CloseBehavior::Immediate).unwrap();
    network
        .close(&listener_fd, CloseBehavior::Immediate)
        .unwrap();
}

#[test]
fn test_bidirectional_tcp_communication_default() {
    let litebox = LiteBox::new(MockPlatform::new());
    let network = Network::new(&litebox);
    bidi_tcp_comms(network, |_| {});
}

#[test]
fn test_bidirectional_tcp_communication_manual() {
    let litebox = LiteBox::new(MockPlatform::new());
    let mut network = Network::new(&litebox);
    network.set_platform_interaction(PlatformInteraction::Manual);
    bidi_tcp_comms(network, |nw| {
        while nw.perform_platform_interaction().call_again_immediately() {}
    });
}

#[test]
fn test_bidirectional_tcp_communication_automatic() {
    let litebox = LiteBox::new(MockPlatform::new());
    let mut network = Network::new(&litebox);
    network.set_platform_interaction(PlatformInteraction::Automatic);
    bidi_tcp_comms(network, |_| {});
}

/// Simulates the exact torn state a dead lock holder can leave behind (see
/// `Network::reset_after_poisoning`'s own doc comment): a live, bound listening socket occupying
/// both a `socket_set` slot AND a `local_port_allocator` entry, as if the process that created it
/// died mid-`net_lock` critical section right after. `reset_after_poisoning` must leave the
/// `Network` fully usable afterward -- zero live sockets, the port free again, and no panic --
/// exactly the guarantee `GlobalStateHandle::net_lock` relies on before handing a lock recovered
/// from a dead holder to its next caller.
#[test]
fn test_reset_after_poisoning_clears_torn_state_and_frees_ports() {
    let litebox = LiteBox::new(MockPlatform::new());
    let mut network = Network::new(&litebox);

    let listen_addr = SocketAddr::V4(SocketAddrV4::from_str("10.0.0.2:8080").unwrap());
    let listener_fd = network
        .socket(Protocol::Tcp)
        .expect("Failed to create TCP socket");
    network
        .bind(&listener_fd, &listen_addr)
        .expect("Failed to bind TCP socket");
    network
        .listen(&listener_fd, 1)
        .expect("Failed to listen on TCP socket");

    // Simulate a dead-holder recovery landing right here, mid-way through whatever the (now dead)
    // holder was doing with this socket -- `reset_after_poisoning` must not need `listener_fd`
    // (or any other previously-issued fd/port) to be dropped cleanly first; it wholesale resets
    // regardless of what state the caller was in, exactly as `net_lock` calls it before the next
    // caller ever touches the guard.
    network.reset_after_poisoning();

    assert_eq!(
        network.socket_set.iter().count(),
        0,
        "reset_after_poisoning must leave socket_set fully empty"
    );

    // Port 8080 must be free again -- if `local_port_allocator` were NOT reset, this bind would
    // fail with `AlreadyInUse` even though the socket that held it is long gone.
    let new_listener_fd = network
        .socket(Protocol::Tcp)
        .expect("Failed to create TCP socket after reset");
    network
        .bind(&new_listener_fd, &listen_addr)
        .expect("port 8080 must be free again after reset_after_poisoning");
    network
        .listen(&new_listener_fd, 1)
        .expect("Failed to listen on TCP socket after reset");

    // A full connect/accept/send/receive cycle must work normally on the fresh state -- proves
    // `reset_after_poisoning` didn't just clear counters while leaving `socket_set`/`interface` in
    // some inconsistent in-between shape.
    let client_fd = network
        .socket(Protocol::Tcp)
        .expect("Failed to create TCP socket");
    let err = network
        .connect(&client_fd, &listen_addr, false)
        .unwrap_err();
    assert!(
        matches!(err, ConnectError::InProgress),
        "Expected InProgress error, got {err:?}",
    );
}

/// Binds 8080 and arms its shared accept queue, returning the row index.
fn arm_listener(network: &mut Network<MockPlatform>) -> usize {
    let listen_addr = SocketAddr::V4(SocketAddrV4::from_str("10.0.0.2:8080").unwrap());
    let listener_fd = network
        .socket(Protocol::Tcp)
        .expect("Failed to create TCP socket");
    network
        .bind(&listener_fd, &listen_addr)
        .expect("Failed to bind TCP socket");
    network
        .listen(&listener_fd, 1)
        .expect("Failed to listen on TCP socket");
    network
        .listen_queue_index(8080)
        .expect("listen() armed a shared accept queue row for 8080")
}

/// The 1-in-512 sweep that frees a listening port's shared accept queue once no LIVE referent
/// names it any more may only retire a row it has LIVENESS INFORMATION about.
///
/// Three arms, one binary, each of which fails if the guard is wrong in that direction:
///
/// * `unknown` -- no referent pid was recorded at all, because `SystemInfoProvider::current_pid`
///   answered `0` (the trait default, and this crate's own `MockPlatform`) and
///   `ListenQueue::record_referent` declines it. The row must STAY armed: reading "no live
///   referent" out of "no referent known" retired a port whose owner had just armed it and was
///   about to `accept` on it, which is exactly the deaf-port shape this sweep exists to clean up
///   after. Live: `test_bidirectional_tcp_communication_manual` hung in `accept` forever, its port
///   retired on the first tick that swept it.
/// * `live` -- a recorded referent that is alive: must stay armed (the ordinary case).
/// * `dead` -- a recorded referent that is gone: MUST be retired, or the guard above is a blanket
///   "never sweep" and the leaked-row reason for this sweep comes straight back.
#[test]
fn test_reclaim_orphaned_listen_queue_needs_a_known_dead_referent() {
    const REFERENT: u32 = 0x5eed;

    // (1) unknown: no pid recorded -> stays armed.
    let platform = MockPlatform::new();
    let litebox = LiteBox::new(platform);
    let mut network = Network::new(&litebox);
    let index = arm_listener(&mut network);
    network.listen_queues[index].ref_pids = [0; MAX_QUEUE_REF_OWNERS];
    // The sweep runs on the tick whose counter is a multiple of 512, so land one there.
    network.reclaim_tick = 512;
    network.maintain_listening_queues();
    assert_eq!(
        network.listen_queue_index(8080),
        Some(index),
        "a port nobody closed must stay armed even when this platform cannot name a referent"
    );

    // (2) live: a referent that is alive -> stays armed.
    let platform = MockPlatform::new();
    let litebox = LiteBox::new(platform);
    let mut network = Network::new(&litebox);
    let index = arm_listener(&mut network);
    network.listen_queues[index].ref_pids = [0; MAX_QUEUE_REF_OWNERS];
    network.listen_queues[index].ref_pids[0] = REFERENT;
    network.reclaim_tick = 512;
    network.maintain_listening_queues();
    assert_eq!(
        network.listen_queue_index(8080),
        Some(index),
        "a port with a live referent must stay armed"
    );

    // (3) dead: every referent gone -> retired, the row freed for another port.
    let platform = MockPlatform::new();
    let litebox = LiteBox::new(platform);
    let mut network = Network::new(&litebox);
    let index = arm_listener(&mut network);
    network.listen_queues[index].ref_pids = [0; MAX_QUEUE_REF_OWNERS];
    network.listen_queues[index].ref_pids[0] = REFERENT;
    platform.mark_dead(REFERENT);
    network.reclaim_tick = 512;
    network.maintain_listening_queues();
    assert_eq!(
        network.listen_queue_index(8080),
        None,
        "a port whose only referent died without closing must be reclaimed"
    );
}
