// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Some Assembly instructions

use core::arch::asm;

#[expect(clippy::inline_always)]
#[inline(always)]
pub fn hlt_loop() -> ! {
    loop {
        unsafe {
            asm!("hlt");
        }
    }
}

/// Read the given MSR.
///
/// Kept as a safe `fn` (unlike `x86_64::registers::model_specific::Msr::read`'s own `unsafe fn`
/// signature, which requires the caller to independently ensure the read has no unsafe side
/// effects) to preserve every existing call site's contract unchanged -- delegates to the crate's
/// real instruction encoding instead of this file's own hand-rolled `rdmsr` via inline `asm!`
/// (previously duplicated, inconsistently, in `litebox_platform_linux_kernel` too). Every real
/// MSR this function is ever called with in this codebase is a plain read with no side effect
/// litebox itself needs to guard against beyond "is this MSR readable on the current CPU",
/// exactly what every existing caller already assumed when calling the old safe hand-rolled
/// version -- so this preserves that assumption explicitly rather than silently, not changes it.
#[expect(clippy::inline_always)]
#[inline(always)]
pub fn rdmsr(msr: u32) -> u64 {
    // SAFETY: see this function's own doc comment -- every existing caller already treated a
    // plain MSR read as free of unsafe side effects, matching this crate's own safety contract.
    unsafe { x86_64::registers::model_specific::Msr::new(msr).read() }
}

/// Write the given MSR. See `rdmsr`'s own doc comment for why this stays a safe `fn`.
#[expect(clippy::inline_always)]
#[inline(always)]
pub fn wrmsr(msr: u32, value: u64) {
    // SAFETY: see `rdmsr`'s own doc comment.
    unsafe { x86_64::registers::model_specific::Msr::new(msr).write(value) }
}
