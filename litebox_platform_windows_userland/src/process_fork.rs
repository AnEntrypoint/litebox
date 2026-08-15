// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Diagnostic-only, standalone `CreateProcess`-based child-spawn primitive: pass 111 of
//! `scratchpad/jqrepro/FINDINGS.txt`'s investigation into an intermittent, guest-visible crash
//! that passes 107-110 root-caused to today's `fork()` running the child as a `std::thread`
//! inside the SAME host process as the parent, which cannot preserve the parent's own virtual
//! addresses for the child (they are already occupied by the parent's own live mappings).
//!
//! Pass 108 designed the real fix: give the child a genuine separate Windows process (a real
//! `CreateProcess`-spawned address space), then force-allocate each of the parent's reservation
//! groups at the SAME address in the child (empty except for its own freshly-loaded image) and
//! `WriteProcessMemory` the parent's actual bytes into it. Pass 109 empirically validated the
//! mechanism in a standalone throwaway program, against synthetic addresses, and found the one
//! sharp platform constraint that shapes it: `VirtualAlloc2`/`MEM_ADDRESS_REQUIREMENTS` can only
//! force a NEW reservation's base onto a 64KB (`dwAllocationGranularity`) boundary -- which is
//! exactly why pass 110 preserved each reservation GROUP's aligned base (not each individual,
//! frequently sub-granularity-offset guest VMA address) in `litebox::mm::AddressRelocations::
//! group_relocations`.
//!
//! This module is pass 111's first LIVE exercise of that mechanism: [`diagnostic_spawn_and_copy`]
//! spawns a real, `CREATE_SUSPENDED` child process re-executing this same litebox runner binary
//! (so the child has the litebox runtime linked in and ready, though it is never actually resumed
//! into it -- see below), forces an allocation at each of the caller's `group_relocations` bases,
//! `WriteProcessMemory`s the corresponding source bytes into it, and reports success/failure per
//! group. It is gated behind `LITEBOX_DIAG_PROCESS_FORK_SPAWN=1` and NEVER runs otherwise; even
//! when it runs, it is purely a side experiment that runs ALONGSIDE the real, unmodified,
//! thread-based `fork()` path in `do_clone` -- the guest's actual execution is unaffected. The
//! child is always `TerminateProcess`'d at the end, never resumed: resuming it into real guest
//! execution needs VEH re-registration, fd inheritance, and signal IPC (pass 108 SECTION 1, Q1/
//! Q3/Q4), none of which this pass builds. This module proves the memory-reservation-and-copy leg
//! of the design against REAL production guest memory layouts (not pass 109's synthetic
//! addresses) -- it does not yet make fork() itself faster, safer, or address-preserving.
//!
//! # The re-exec marker
//!
//! A `CreateProcess`-spawned child must be distinguishable from an ordinary fresh invocation of
//! the litebox runner (a guest binary path, e.g. `alpine-rootfs.tar`'s own `/usr/bin/python3`,
//! must never be misread as this marker). [`REEXEC_CHILD_ENV_VAR`] is an internal-only
//! environment variable litebox's own re-exec sets and checks for -- never guest-visible, never
//! part of any documented CLI surface. This pass's child never actually branches on it (it is
//! `CREATE_SUSPENDED` and torn down before it would run any litebox startup code that might), but
//! setting it now establishes the marker contract a future resuming implementation will rely on.

use core::ffi::c_void;
use std::ops::Range;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::Memory::{
    MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER, MEM_EXTENDED_PARAMETER_0,
    MEM_EXTENDED_PARAMETER_1, MEM_RELEASE, MEM_RESERVE, MemExtendedParameterAddressRequirements,
    PAGE_READWRITE, VirtualFreeEx,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess,
};

/// Internal-only marker env var: set (never read as guest-visible) on a `CreateProcess`-spawned
/// litebox child so a future resuming implementation can distinguish "I am being set up as a
/// forked child" from "I am a normal fresh litebox invocation". Not consulted by this pass's own
/// diagnostic (the child is always torn down suspended, before any code that would check it
/// runs) -- set here only to establish the contract early, per this module's own doc comment.
const REEXEC_CHILD_ENV_VAR: &str = "LITEBOX_INTERNAL_FORK_CHILD";

/// Whether the pass-111 `CreateProcess`-based fork diagnostic is enabled
/// (`LITEBOX_DIAG_PROCESS_FORK_SPAWN=1`). Never runs otherwise -- see this module's doc comment.
#[must_use]
pub fn diag_process_fork_spawn_enabled() -> bool {
    std::env::var_os("LITEBOX_DIAG_PROCESS_FORK_SPAWN").is_some()
}

/// Per-group outcome reported by [`diagnostic_spawn_and_copy`].
pub struct GroupCopyResult {
    /// The group's span in the (real, live) parent address space, exactly as `Vmem::duplicate`
    /// recorded it.
    pub source_group: Range<usize>,
    /// Whether reserving this exact span at this exact address in the fresh child process, then
    /// `WriteProcessMemory`-ing the parent's real bytes into it, succeeded.
    pub succeeded: bool,
    /// `GetLastError()` from whichever Win32 call failed, if `succeeded` is `false`.
    pub last_error: u32,
}

/// RAII guard for the diagnostic child process's handles: guarantees `TerminateProcess` +
/// `CloseHandle` run on every exit path (including an early `?`-propagated failure partway
/// through the per-group loop), matching this pass's "never resume the child" invariant even
/// when something goes wrong before the loop finishes.
struct SuspendedChildGuard {
    process: HANDLE,
    thread: HANDLE,
}

impl Drop for SuspendedChildGuard {
    fn drop(&mut self) {
        // Best-effort: the child is CREATE_SUSPENDED and has executed no guest or litebox-runtime
        // code, so there is nothing to fail cleanly -- an unconditional TerminateProcess is the
        // whole intended cleanup, not a fallback path.
        unsafe {
            TerminateProcess(self.process, 0);
            CloseHandle(self.thread);
            CloseHandle(self.process);
        }
    }
}

/// Spawns a real, `CREATE_SUSPENDED` Windows child process re-executing the current litebox
/// runner binary, then for each `(source_group, _)` pair in `group_relocations` (as produced by
/// `litebox::mm::AddressRelocations::group_relocations`, i.e. pass 110's per-reservation-group
/// aligned-base bookkeeping) attempts to:
///
///  1. Force-reserve+commit `source_group`'s exact span at the SAME address in the child, via
///     `VirtualAllocEx` + `MEM_ADDRESS_REQUIREMENTS` (a hard requirement, not a hint -- see pass
///     109's finding that a plain hint can silently round to the wrong address with no error).
///  2. `WriteProcessMemory` `read_source_bytes(source_group)`'s bytes into that reservation.
///
/// The child is `TerminateProcess`'d (never resumed) when this function returns, regardless of
/// outcome -- see this module's doc comment for why resuming it is out of scope for this pass.
///
/// `read_source_bytes` reads the CALLER's (parent's) own live memory at the given range -- the
/// caller supplies it rather than this function reaching into `litebox::platform::RawConstPointer`
/// directly, keeping this platform-crate module free of a dependency on `litebox`'s guest-memory
/// abstraction it does not otherwise need.
///
/// Returns one [`GroupCopyResult`] per input group, in the same order, even if some fail -- a
/// failure on one group does not abort the rest, so the diagnostic reports the FULL picture for
/// this fork() call's actual guest memory layout, not just the first failure.
pub fn diagnostic_spawn_and_copy(
    group_relocations: &[(Range<usize>, usize)],
    mut read_source_bytes: impl FnMut(Range<usize>) -> Option<Vec<u8>>,
) -> Result<Vec<GroupCopyResult>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe() failed: {e}"))?;
    let mut exe_wide: Vec<u16> = exe
        .as_os_str()
        .encode_wide_for_windows()
        .chain(std::iter::once(0))
        .collect();

    // Mark the child, for a future resuming implementation's benefit -- see
    // `REEXEC_CHILD_ENV_VAR`'s doc comment. `CreateProcessW`'s `lpEnvironment: null` means
    // "inherit the parent's environment block", so setting this on the CURRENT process's
    // environment before spawning (and clearing it right after, since this diagnostic's own
    // process must not appear to be a fork child to anything else checking this var) is the
    // simplest correct way to propagate it without hand-building a full environment block.
    // Safety: `std::env::set_var`/`remove_var` are documented as not thread-safe against
    // concurrent reads of the environment on this platform; `do_clone`'s caller already holds
    // whatever process-wide serialization real fork() requires (see `Vmem::duplicate`'s own
    // safety contract: "no other code is concurrently mutating memory ... other threads must be
    // stopped"), so this diagnostic, invoked from exactly that same call site, inherits the same
    // guarantee.
    unsafe {
        std::env::set_var(REEXEC_CHILD_ENV_VAR, "1");
    }
    let spawn_result = spawn_suspended(&mut exe_wide);
    unsafe {
        std::env::remove_var(REEXEC_CHILD_ENV_VAR);
    }
    let (process, thread) = spawn_result?;
    let guard = SuspendedChildGuard { process, thread };

    let mut results = Vec::with_capacity(group_relocations.len());
    for (source_group, dest_base) in group_relocations {
        results.push(copy_one_group(
            guard.process,
            source_group,
            *dest_base,
            &mut read_source_bytes,
        ));
    }
    // `guard` drops here: TerminateProcess + CloseHandle, unconditionally.
    Ok(results)
}

/// Trait-free helper: `OsStr::encode_wide` is already available via
/// `std::os::windows::ffi::OsStrExt`, imported locally to keep this module's top-level `use`
/// block Windows-specific-import-free at a glance (this whole crate is Windows-only per its
/// crate-level `#![cfg(...)]`, but keeping the platform-specific extension trait import scoped to
/// its one call site matches this module's own narrow, diagnostic-only footprint).
trait EncodeWideExt {
    fn encode_wide_for_windows(&self) -> std::os::windows::ffi::EncodeWide<'_>;
}
impl EncodeWideExt for std::ffi::OsStr {
    fn encode_wide_for_windows(&self) -> std::os::windows::ffi::EncodeWide<'_> {
        use std::os::windows::ffi::OsStrExt as _;
        self.encode_wide()
    }
}

fn spawn_suspended(exe_wide: &mut [u16]) -> Result<(HANDLE, HANDLE), String> {
    let mut startup_info: STARTUPINFOW = unsafe { core::mem::zeroed() };
    startup_info.cb =
        u32::try_from(core::mem::size_of::<STARTUPINFOW>()).expect("STARTUPINFOW fits in u32");
    let mut process_info: PROCESS_INFORMATION = unsafe { core::mem::zeroed() };

    let ok = unsafe {
        CreateProcessW(
            core::ptr::null(),
            exe_wide.as_mut_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            0,
            CREATE_SUSPENDED,
            core::ptr::null(),
            core::ptr::null(),
            &raw const startup_info,
            &raw mut process_info,
        )
    };
    if ok == 0 {
        return Err(format!(
            "CreateProcessW(CREATE_SUSPENDED) failed: GetLastError={}",
            unsafe { GetLastError() }
        ));
    }
    Ok((process_info.hProcess, process_info.hThread))
}

/// Attempts steps 1-2 of [`diagnostic_spawn_and_copy`]'s doc comment for a single reservation
/// group. Never panics on failure -- a failed group is reported in the returned
/// [`GroupCopyResult`], not propagated as an error, so the caller sees every group's outcome.
fn copy_one_group(
    child: HANDLE,
    source_group: &Range<usize>,
    _dest_base: usize,
    read_source_bytes: &mut impl FnMut(Range<usize>) -> Option<Vec<u8>>,
) -> GroupCopyResult {
    let len = source_group.len();
    let fail = |err: u32| GroupCopyResult {
        source_group: source_group.clone(),
        succeeded: false,
        last_error: err,
    };

    // Step 1: force-reserve+commit the group's exact span, at its exact SOURCE address, in the
    // child. `MEM_ADDRESS_REQUIREMENTS` makes this a hard requirement -- per pass 109, either it
    // lands EXACTLY here or the call fails outright (ERROR_INVALID_PARAMETER if the address isn't
    // 64KB-aligned, which a reservation-GROUP base per pass 110's own bookkeeping always is;
    // otherwise typically ERROR_INVALID_ADDRESS if something in the child's own fresh image/DLL
    // layout already occupies it -- exactly the collision class this diagnostic exists to check
    // for against REAL, not synthetic, guest addresses).
    let mut addr_req = MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: source_group.start as *mut c_void,
        HighestEndingAddress: (source_group.end - 1) as *mut c_void,
        Alignment: 0,
    };
    let mut ext_param = MEM_EXTENDED_PARAMETER {
        Anonymous1: MEM_EXTENDED_PARAMETER_0 {
            _bitfield: MemExtendedParameterAddressRequirements as u64,
        },
        Anonymous2: MEM_EXTENDED_PARAMETER_1 {
            Pointer: (&raw mut addr_req).cast::<c_void>(),
        },
    };
    // `VirtualAllocEx` is the cross-process sibling of `VirtualAlloc2` used elsewhere in this
    // crate (see `lib.rs`'s `reserve_and_commit`); `windows-sys` 0.60.2's `VirtualAllocEx`
    // signature does not itself take extended parameters, so route through the raw
    // `VirtualAlloc2` entry point with an explicit process handle instead of the no-handle
    // `VirtualAlloc2`-via-current-process helper `lib.rs` uses -- both are the same underlying
    // Win32 API, `VirtualAlloc2`, just called with a non-current-process handle here.
    let reserved = unsafe {
        windows_sys::Win32::System::Memory::VirtualAlloc2(
            child,
            core::ptr::null_mut(),
            len,
            MEM_RESERVE | MEM_COMMIT,
            PAGE_READWRITE,
            &raw mut ext_param,
            1,
        )
    };
    if reserved.is_null() {
        return fail(unsafe { GetLastError() });
    }
    if reserved as usize != source_group.start {
        // Should be unreachable given `MEM_ADDRESS_REQUIREMENTS` is a hard requirement (pass 109
        // observed silent rounding ONLY under the plain-hint API, never under this one) -- but
        // treat a mismatch as a failure rather than trusting the assumption blindly, since this
        // is exactly the class of bug ("silently landed at the wrong address") the whole design
        // exists to eliminate.
        unsafe {
            VirtualFreeEx(child, reserved, 0, MEM_RELEASE);
        }
        return fail(0);
    }

    // Step 2: copy the parent's real, live bytes for this group into the child's newly reserved
    // span at the identical address.
    let Some(bytes) = read_source_bytes(source_group.clone()) else {
        unsafe {
            VirtualFreeEx(child, reserved, 0, MEM_RELEASE);
        }
        return fail(0);
    };
    debug_assert_eq!(bytes.len(), len, "read_source_bytes returned wrong length");
    let mut written = 0usize;
    let ok = unsafe {
        WriteProcessMemory(
            child,
            reserved,
            bytes.as_ptr().cast::<c_void>(),
            bytes.len(),
            &raw mut written,
        )
    };
    if ok == 0 || written != bytes.len() {
        return fail(unsafe { GetLastError() });
    }

    GroupCopyResult {
        source_group: source_group.clone(),
        succeeded: true,
        last_error: 0,
    }
}
