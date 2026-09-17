// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Handling the allocation of local ports

use core::num::{NonZeroU16, NonZeroU64};

use thiserror::Error;

use crate::utils::rng::FastRng;

/// An allocator for local ports, making sure that no already-allocated ports are given out either
/// in case of ephemeral port allocation, or in the case of asking for a specific port.
pub(crate) struct LocalPortAllocator {
    // refcount[port - 1] holds port `port`'s reference count (0 == free). A fixed, pointer-free
    // 65535-entry array, NOT a `HashMap` (as this used to be) -- `Network` (and this field inline
    // within it) now lives in the cross-process shared kernel arena as of the `socket_set`
    // shared-arena-native fix (`GlobalState`'s `net: Mutex<Network<Platform>>` field, placed via
    // `SharedKernelStateSlot::ShimGlobalState`). A `HashMap`'s backing table is a SEPARATE
    // allocation on the constructing process's private heap, reachable only through a raw pointer
    // stored inline in the map -- a cross-process-forked child that ATTACHES to (rather than
    // constructs) the shared `GlobalState` reads that same pointer VALUE, meaningless in its own
    // address space, so `hashbrown`'s SIMD probe loop reads garbage control bytes with no
    // guaranteed EMPTY sentinel and can spin forever. Confirmed live via cdb CPU sampling
    // (2026-09-17, `.wfgy/cpu_profile_session`): a forked child burned an entire CPU core for 6+
    // real minutes, RIP always inside this exact hashbrown group-scan code
    // (`LocalPortAllocator::ephemeral_port`/`deallocate`) -- the same "stale cross-process
    // pointer" bug class already fixed a dozen times elsewhere today (see this crate's
    // `MAX_SOCKETS` doc comment / AGENTS.md), just not yet audited for this nested field.
    refcount: [u16; Self::PORT_COUNT],
    rng: FastRng,
}

impl Default for LocalPortAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalPortAllocator {
    /// Number of valid port values (1..=65535); index `port.get() - 1` into `refcount`.
    const PORT_COUNT: usize = u16::MAX as usize;

    /// Sets up a new local port allocator
    pub(crate) fn new() -> Self {
        Self {
            refcount: [0; Self::PORT_COUNT],
            rng: FastRng::new_from_seed(NonZeroU64::new(0x13374a4159421337).unwrap()),
        }
    }

    fn index(port: NonZeroU16) -> usize {
        (port.get() - 1) as usize
    }

    /// Allocate a new ephemeral local port (i.e., port in the range 49152 and 65535)
    pub(crate) fn ephemeral_port(&mut self) -> Result<LocalPort, LocalPortAllocationError> {
        for _ in 0..100 {
            let port =
                NonZeroU16::new(u16::try_from(self.rng.next_in_range_u32(49152..65536)).unwrap())
                    .unwrap();
            if let Ok(local_port) = self.specific_port(port) {
                return Ok(local_port);
            }
        }
        // If we haven't yet found a port after 100 tries, it is highly likely lots of ports are
        // already in use, so we should start looking over them one by one
        for port in 49152..=65535 {
            let port = NonZeroU16::new(port).unwrap();
            if let Ok(local_port) = self.specific_port(port) {
                return Ok(local_port);
            }
        }
        // If we _still_ haven't found any, then we have run out of ports to give out
        Err(LocalPortAllocationError::NoAvailableFreePorts)
    }

    /// Allocate a specific local port, if available
    pub(crate) fn specific_port(
        &mut self,
        port: NonZeroU16,
    ) -> Result<LocalPort, LocalPortAllocationError> {
        let slot = &mut self.refcount[Self::index(port)];
        if *slot != 0 {
            Err(LocalPortAllocationError::AlreadyInUse(port.get()))
        } else {
            *slot = 1;
            Ok(LocalPort { port })
        }
    }

    /// Allocate a local port, either ephemeral (if `port` is 0) or specific (if `port` is non-zero)
    pub(crate) fn allocate_local_port(
        &mut self,
        port: u16,
    ) -> Result<LocalPort, LocalPortAllocationError> {
        let Some(port) = NonZeroU16::new(port) else {
            return self.ephemeral_port();
        };
        self.specific_port(port)
    }

    /// Increments the ref-count for a local port, producing a new [`LocalPort`] token to be used
    #[must_use]
    pub(crate) fn allocate_same_local_port(&mut self, port: &LocalPort) -> LocalPort {
        let slot = &mut self.refcount[Self::index(port.port)];
        if *slot == 0 {
            // Because we have a `LocalPort`, it is (as an invariant) impossible to have the value
            // be missing from the refcount.
            unreachable!()
        }
        // We just bump the refcount, making sure there is no overflow, and then produce the new
        // `LocalPort` token.
        *slot = slot.checked_add(1).unwrap();
        LocalPort { port: port.port }
    }

    /// Consumes a [`LocalPort`], possibly marking it as available again.
    pub(crate) fn deallocate(&mut self, port: LocalPort) {
        let slot = &mut self.refcount[Self::index(port.port)];
        match *slot {
            0 => unreachable!(),
            1 => *slot = 0,
            n => *slot = n - 1,
        }
    }

    /// Deallocate a port number tracked by this allocator.
    pub(crate) fn deallocate_port(&mut self, port: u16) {
        if let Some(port) = NonZeroU16::new(port) {
            self.deallocate(LocalPort { port });
        }
    }
}

/// A token expressing ownership over a specific local port.
///
/// Explicitly not cloneable/copyable.
pub(crate) struct LocalPort {
    port: NonZeroU16,
}

impl LocalPort {
    pub(crate) fn port(&self) -> u16 {
        self.port.get()
    }
}

/// Errors that could be returned when allocating a port
#[derive(Debug, Clone, Copy, Error)]
pub enum LocalPortAllocationError {
    #[error("Port {0} is already in use")]
    AlreadyInUse(u16),
    #[error("No free ports are available")]
    NoAvailableFreePorts,
}
