// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Socket ring-buffer storage that lives in the shared kernel arena.
//!
//! `Network` (and its `smoltcp` socket table) is one object shared by every process of a
//! cross-process-fork family, and every process runs the network poll over ALL sockets. A socket's
//! RX/TX buffers used to be ordinary `Vec`s on the CREATING process's private heap, so any other
//! process polling that socket dereferenced a pointer meaningless in its own address space (a
//! `STATUS_ACCESS_VIOLATION` in `RingBuffer::write_unallocated` the moment a second process
//! touched a TCP socket the first had created). Buffers are instead carved from fixed slot pools
//! placed once, at `Network::new`, in the shared kernel arena (see
//! `SharedKernelStateProvider::shared_kernel_arena_alloc_bytes`: session-lifetime, never freed --
//! which is why this is a pool with a free bitmap rather than per-socket allocation). All access
//! is under the `Network` lock.

use core::alloc::Layout;

use smoltcp::iface::SocketHandle;
use smoltcp::socket::udp;
use smoltcp::storage::{PacketBuffer, PacketMetadata, RingBuffer};

use super::{MAX_PACKET_COUNT, MAX_SOCKETS, SOCKET_RING_SIZE};
use crate::platform::SharedKernelStateProvider;

// Two data slots per TCP socket (rx + tx), so `MAX_DATA_SLOTS / 2` TCP sockets fit: sized to
// `MAX_SOCKETS` so the pool stops being the binding limit, at the same 16 MiB the old 256x64KiB
// pool cost -- see `SOCKET_RING_SIZE`'s own doc comment for the measurement behind it.
const MAX_DATA_SLOTS: usize = 2 * MAX_SOCKETS;
const MAX_META_SLOTS: usize = 64;
const META_SLOT_SIZE: usize = 4096;
const SLOT_ALIGN: usize = 4096;

const _: () = assert!(
    core::mem::size_of::<PacketMetadata<udp::UdpMetadata>>() * MAX_PACKET_COUNT <= META_SLOT_SIZE
);
const _: () = assert!(core::mem::align_of::<PacketMetadata<udp::UdpMetadata>>() <= SLOT_ALIGN);

struct Pool<const N: usize> {
    base: usize,
    slot_size: usize,
    slots: usize,
    used: [bool; N],
}

impl<const N: usize> Pool<N> {
    /// Places up to `N` slots in the shared arena, halving the request until the arena can hold
    /// it. A pool that gets nothing has zero slots, so socket creation fails with an ordinary
    /// error instead of panicking.
    fn new<P: SharedKernelStateProvider>(platform: &P, slot_size: usize) -> Self {
        let mut slots = N;
        while slots > 0 {
            if let Ok(layout) = Layout::from_size_align(slots * slot_size, SLOT_ALIGN)
                && let Some(ptr) = platform.shared_kernel_arena_alloc_bytes(layout)
            {
                return Self {
                    base: ptr.as_ptr() as usize,
                    slot_size,
                    slots,
                    used: [false; N],
                };
            }
            slots /= 2;
        }
        Self {
            base: 0,
            slot_size,
            slots: 0,
            used: [false; N],
        }
    }

    fn alloc(&mut self) -> Option<(u16, &'static mut [u8])> {
        let idx = self.used[..self.slots].iter().position(|used| !*used)?;
        self.used[idx] = true;
        // SAFETY: `base..base + slots * slot_size` is a live shared-arena allocation that is never
        // freed, `idx < slots`, and `used[idx]` was just set so no other `&mut` to this slot
        // exists until `free` clears it.
        let slot = unsafe {
            core::slice::from_raw_parts_mut(
                (self.base + idx * self.slot_size) as *mut u8,
                self.slot_size,
            )
        };
        slot.fill(0);
        Some((u16::try_from(idx).ok()?, slot))
    }

    fn free(&mut self, idx: u16) {
        if let Some(used) = self.used.get_mut(usize::from(idx)) {
            *used = false;
        }
    }

    fn reset(&mut self) {
        self.used = [false; N];
    }
}

/// The pool slots one socket holds.
#[derive(Clone, Copy)]
pub(crate) struct Claim {
    data: [u16; 4],
    ndata: u8,
    meta: [u16; 4],
    nmeta: u8,
}

impl Claim {
    const fn new() -> Self {
        Self {
            data: [0; 4],
            ndata: 0,
            meta: [0; 4],
            nmeta: 0,
        }
    }
}

pub(crate) struct SocketBuffers {
    data: Pool<MAX_DATA_SLOTS>,
    meta: Pool<MAX_META_SLOTS>,
    owners: [Option<(SocketHandle, Claim)>; MAX_SOCKETS],
}

impl SocketBuffers {
    pub(crate) fn new<P: SharedKernelStateProvider>(platform: &P) -> Self {
        Self {
            data: Pool::new(platform, SOCKET_RING_SIZE),
            meta: Pool::new(platform, META_SLOT_SIZE),
            owners: core::array::from_fn(|_| None),
        }
    }

    fn data_slot(&mut self, claim: &mut Claim) -> Option<&'static mut [u8]> {
        let (idx, slot) = self.data.alloc()?;
        claim.data[usize::from(claim.ndata)] = idx;
        claim.ndata += 1;
        Some(slot)
    }

    fn meta_slice(&mut self, claim: &mut Claim) -> Option<&'static mut [PacketMetadata<udp::UdpMetadata>]> {
        let (idx, slot) = self.meta.alloc()?;
        claim.meta[usize::from(claim.nmeta)] = idx;
        claim.nmeta += 1;
        let ptr = slot.as_mut_ptr().cast::<PacketMetadata<udp::UdpMetadata>>();
        for i in 0..MAX_PACKET_COUNT {
            // SAFETY: the slot is `META_SLOT_SIZE` bytes, aligned to `SLOT_ALIGN`, and the const
            // assertions above guarantee `MAX_PACKET_COUNT` entries fit with valid alignment.
            unsafe { ptr.add(i).write(PacketMetadata::EMPTY) };
        }
        // SAFETY: `ptr` names `MAX_PACKET_COUNT` initialized entries in a slot only this claim owns.
        Some(unsafe { core::slice::from_raw_parts_mut(ptr, MAX_PACKET_COUNT) })
    }

    /// RX and TX ring buffers for a TCP socket, or `None` when the pool is exhausted.
    pub(crate) fn tcp(&mut self) -> Option<(RingBuffer<'static, u8>, RingBuffer<'static, u8>, Claim)> {
        let mut claim = Claim::new();
        let rx = self.data_slot(&mut claim);
        let tx = self.data_slot(&mut claim);
        match (rx, tx) {
            (Some(rx), Some(tx)) => Some((RingBuffer::new(rx), RingBuffer::new(tx), claim)),
            _ => {
                self.abandon(claim);
                None
            }
        }
    }

    /// RX and TX packet buffers for a UDP socket, or `None` when a pool is exhausted.
    #[expect(clippy::type_complexity, reason = "smoltcp's own buffer types")]
    pub(crate) fn udp(
        &mut self,
    ) -> Option<(
        PacketBuffer<'static, udp::UdpMetadata>,
        PacketBuffer<'static, udp::UdpMetadata>,
        Claim,
    )> {
        let mut claim = Claim::new();
        let rx_meta = self.meta_slice(&mut claim);
        let rx_data = self.data_slot(&mut claim);
        let tx_meta = self.meta_slice(&mut claim);
        let tx_data = self.data_slot(&mut claim);
        match (rx_meta, rx_data, tx_meta, tx_data) {
            (Some(rm), Some(rd), Some(tm), Some(td)) => {
                Some((PacketBuffer::new(rm, rd), PacketBuffer::new(tm, td), claim))
            }
            _ => {
                self.abandon(claim);
                None
            }
        }
    }

    /// Records that `handle` owns `claim`, so removing the socket returns its slots.
    pub(crate) fn adopt(&mut self, handle: SocketHandle, claim: Claim) {
        self.release(handle);
        if let Some(slot) = self.owners.iter_mut().find(|o| o.is_none()) {
            *slot = Some((handle, claim));
        } else {
            self.abandon(claim);
        }
    }

    /// Returns the slots `handle` holds (no-op when it holds none).
    pub(crate) fn release(&mut self, handle: SocketHandle) {
        let claim = self
            .owners
            .iter_mut()
            .find(|o| o.is_some_and(|(h, _)| h == handle))
            .and_then(Option::take);
        if let Some((_, claim)) = claim {
            self.abandon(claim);
        }
    }

    /// Returns a claim's slots to the pools.
    pub(crate) fn abandon(&mut self, claim: Claim) {
        for idx in &claim.data[..usize::from(claim.ndata)] {
            self.data.free(*idx);
        }
        for idx in &claim.meta[..usize::from(claim.nmeta)] {
            self.meta.free(*idx);
        }
    }

    /// Frees everything (the whole socket table was just wiped).
    pub(crate) fn reset(&mut self) {
        self.data.reset();
        self.meta.reset();
        self.owners = core::array::from_fn(|_| None);
    }

    /// `(data_granted, data_used, meta_granted, meta_used, owners)`.
    ///
    /// `data_granted`/`meta_granted` are what the arena actually gave this pool, which
    /// [`Pool::new`] halves until the allocation fits and which can therefore be far below
    /// `MAX_DATA_SLOTS`/`MAX_META_SLOTS` under arena pressure -- a refill that fails "because the
    /// pool is exhausted" can mean the pool was never bigger than a handful of slots.
    pub(crate) fn occupancy(&self) -> (usize, usize, usize, usize, usize) {
        (
            self.data.slots,
            self.data.used[..self.data.slots].iter().filter(|u| **u).count(),
            self.meta.slots,
            self.meta.used[..self.meta.slots].iter().filter(|u| **u).count(),
            self.owners.iter().filter(|o| o.is_some()).count(),
        )
    }
}
