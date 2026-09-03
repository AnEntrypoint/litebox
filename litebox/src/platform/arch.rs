// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Architecture-specific platform interfaces.
//!
//! As it currently stands, the interfaces here are only considered for x86-64 and aarch64, in
//! the future other architectures might be supported.

use thiserror::Error;

/// A provider of architecture-specific functionality.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub trait ArchSpecificProvider {
    /// Get the architecture-specific `reg`, for the current guest context.
    ///
    /// Broadly speaking, the platform may use some architecture-specific registers for its own
    /// purposes, and the guest may not be able to directly access or work with them. This function
    /// (along with [`Self::set_arch_specific_register`]) provides the special handling for such
    /// registers. This allows the shim, on behalf of the guest, consistently handle such registers
    /// without needing to worry about platform-specifics.
    fn get_arch_specific_register(
        &self,
        reg: &ArchSpecificRegister,
    ) -> Result<usize, ArchSpecificError>;

    /// Set the architecture-specific `reg` to `val`, for the current guest context.
    ///
    /// See [`Self::get_arch_specific_register`] for details.
    fn set_arch_specific_register(
        &self,
        reg: &ArchSpecificRegister,
        val: usize,
    ) -> Result<(), ArchSpecificError>;

    /// Read the current guest thread's floating-point/SIMD register state into `out`.
    ///
    /// This exists so a shim's signal-delivery path can snapshot the guest's FP/SIMD registers
    /// into the signal frame it hands the guest's handler, the same way real Linux does -- `Err`
    /// (default: unsupported) means the shim must fall back to real Linux's own null-`fpstate`
    /// convention (no FP state saved this delivery) rather than fabricate a value.
    fn get_fp_state(&self, out: &mut [u8]) -> Result<(), ArchSpecificError> {
        let _ = out;
        Err(ArchSpecificError::RegisterUnsupported)
    }

    /// Write `state` back into the current guest thread's floating-point/SIMD registers, the
    /// inverse of [`Self::get_fp_state`]. `state` must be the exact bytes a prior `get_fp_state`
    /// call on this same platform produced (or the platform's own null/identity state) -- this is
    /// not a generic register-file loader, it exists solely to restore what signal delivery
    /// captured.
    fn set_fp_state(&self, state: &[u8]) -> Result<(), ArchSpecificError> {
        let _ = state;
        Err(ArchSpecificError::RegisterUnsupported)
    }
}

/// Architecture-specific registers.
///
/// Implementations of [`ArchSpecificProvider`] can choose to support any subset of these registers,
/// and are not required to support any of them, although this may (unsurprisingly) lead to reduced
/// functionality of certain shims.
#[cfg(target_arch = "x86_64")]
#[non_exhaustive]
pub enum ArchSpecificRegister {
    FsBase,
    GsBase,
}

/// Architecture-specific registers for AArch64.
#[cfg(target_arch = "aarch64")]
#[non_exhaustive]
pub enum ArchSpecificRegister {
    /// `TPIDR_EL0`, the user-mode thread-ID/TLS-base register -- the aarch64 analogue of
    /// x86_64's `FsBase`, set via the `set_tls` syscall (`arch_prctl` does not exist on
    /// aarch64).
    TpidrEl0,
}

/// Errors that can be produced by a [`ArchSpecificProvider`] operation.
#[non_exhaustive]
#[derive(Error, Debug)]
pub enum ArchSpecificError {
    #[error("register is (currently) not supported on the platform")]
    RegisterUnsupported,
    #[error("register is reserved by the platform and access is not allowed")]
    RegisterReserved,
    #[error("register value is outside the permitted range")]
    RegisterUnpermittedValue,
}
