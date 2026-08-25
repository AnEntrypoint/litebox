// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! This module contains the loader for the LiteBox shim.

pub mod auxv;
pub mod elf;
mod stack;

pub(crate) const DEFAULT_STACK_SIZE: usize = 8 * 1024 * 1024; // 8 MB

/// A default low address is used for the binary (which grows upwards) to avoid
/// conflicts with the kernel's memory mappings (which grows downwards).
///
/// On Apple Silicon, `[0, 4 GiB)` is permanently reserved as the `__PAGEZERO`
/// segment (see `litebox_platform_macos_userland`'s own `TASK_ADDR_MIN`), so
/// the customary low Linux address is unusable there -- an `ET_EXEC` binary
/// linked at this address genuinely cannot be loaded at its preferred address
/// on this host (see `docs/macos.md`). This picks the lowest address above
/// that reservation instead, matching the same "low, grows upward" intent.
#[cfg(not(target_vendor = "apple"))]
pub(crate) const DEFAULT_LOW_ADDR: usize = 0x1000_0000;
#[cfg(target_vendor = "apple")]
pub(crate) const DEFAULT_LOW_ADDR: usize = 0x1_0000_0000;
