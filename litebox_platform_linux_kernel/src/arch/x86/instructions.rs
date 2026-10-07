// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use core::arch::asm;

/// Read MSR.
///
/// Kept as a safe `fn` (matching every existing call site's own assumption, unlike
/// `x86_64::registers::model_specific::Msr::read`'s own `unsafe fn` signature) -- delegates to
/// the crate's real instruction encoding instead of this file's own hand-rolled `rdmsr` via
/// inline `asm!` (previously duplicated, inconsistently, in `litebox_platform_lvbs` too).
#[inline]
pub fn rdmsr(msr: u32) -> u64 {
    // SAFETY: every existing caller already treated a plain MSR read as free of unsafe side
    // effects, matching this crate's own safety contract; see this function's doc comment.
    unsafe { x86_64::registers::model_specific::Msr::new(msr).read() }
}

/// Write to MSR a given value. See `rdmsr`'s own doc comment for why this stays a safe `fn`.
#[inline]
pub fn wrmsr(msr: u32, value: u64) {
    // SAFETY: see `rdmsr`'s own doc comment.
    unsafe { x86_64::registers::model_specific::Msr::new(msr).write(value) }
}

#[inline]
pub fn vc_vmgexit() {
    unsafe {
        asm!("rep vmmcall", options(nomem, nostack, preserves_flags));
    }
}

/// Read `CR3` (the current page-table base address, verbatim -- both the physical-address bits
/// AND the low flag bits together, matching this function's own prior hand-rolled behavior).
///
/// `x86_64::registers::control::Cr3::read_raw()` already reads the real register via the same
/// instruction, but splits its result into `(PhysFrame, u16)` (masking the address away from the
/// low flag bits) rather than returning the complete raw value this function's own callers
/// expect -- reassemble the exact same u64 this function always returned, from the crate's own
/// split components, so every existing caller's contract stays unchanged.
#[inline]
pub fn cr3() -> u64 {
    let (frame, flags) = x86_64::registers::control::Cr3::read_raw();
    frame.start_address().as_u64() | u64::from(flags)
}

/// Read `CR2` (the faulting address on the most recent page fault).
///
/// Delegates to `x86_64::registers::control::Cr2::read_raw` (already a workspace dependency)
/// instead of this file's own hand-rolled `mov {}, cr2` via inline `asm!`.
#[inline]
pub fn cr2() -> u64 {
    x86_64::registers::control::Cr2::read_raw()
}
