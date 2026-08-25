// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! A [LiteBox platform](../litebox/platform/index.html) for running LiteBox on
//! userland macOS running on Apple Silicon.
//!
//! # Why aarch64 only
//!
//! There is no x86-64 variant of this platform. Running an x86-64 guest on an
//! Apple Silicon host would mean instruction emulation, which is exactly what
//! LiteBox exists to avoid: the whole point of the "South" platform interface is
//! that the guest's instructions are the host's instructions and only the
//! *system* interface is virtualized. An aarch64 Linux guest on an aarch64 macOS
//! host needs no emulation at all -- just this platform plus the aarch64 syscall
//! rewriting that `litebox_syscall_rewriter` already performs.
//!
//! # How Darwin differs from the other userland platforms
//!
//! * **16 KiB pages.** Apple Silicon's page size is 16 KiB, not 4 KiB, so every
//!   fixed mapping and every protection change must be 16 KiB aligned. This is
//!   why `litebox::mm::linux::PAGE_SIZE` is target-dependent; the guest learns
//!   the same value through `AT_PAGESZ`.
//! * **A 4 GiB `__PAGEZERO`.** The first 4 GiB of an arm64 Mach-O process is
//!   reserved and permanently unmapped, so no guest mapping can live below it.
//!   `MacOsUserland`'s `TASK_ADDR_MIN` reflects that, which in turn means
//!   guest images have to be position-independent or linked above 4 GiB.
//! * **W^X.** Anonymous memory cannot be both writable and executable, and
//!   memory that was ever writable cannot later become executable. The supported
//!   escape hatch is `MAP_JIT`, which requires the host process to be signed with
//!   the `com.apple.security.cs.allow-jit` entitlement and requires writes to be
//!   bracketed by `pthread_jit_write_protect_np`. Executable guest mappings
//!   therefore go through `jit_write_protect`.
//! * **No futex.** Darwin's equivalent is `__ulock_wait`/`__ulock_wake`, which
//!   provides the same compare-and-wait contract.
//! * **No `MAP_FIXED_NOREPLACE`, no `MAP_POPULATE`, no `MAP_GROWSDOWN`.** The
//!   first is emulated with an atomic `mach_vm_allocate` reservation, the second
//!   with `madvise(MADV_WILLNEED)`, and the third has no equivalent.
//! * **No vDSO.** `SystemInfoProvider::get_vdso_address` reports `None`, which
//!   means a guest signal handler must supply its own `sa_restorer`.
//!

// Restrict this crate to macOS on Apple Silicon. See the module docs for why
// there is deliberately no x86-64 variant.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use core::time::Duration;
use std::io::IsTerminal as _;
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use litebox::platform::page_mgmt::{
    AllocationError, DeallocationError, FixedAddressBehavior, MemoryRegionPermissions,
    PermissionUpdateError, SharedMemoryError,
};
use litebox::platform::{ImmediatelyWokenUp, UnblockedOrTimedOut};
use litebox::utils::TruncateExt as _;
use zerocopy::{FromBytes, IntoBytes};

extern crate alloc;

mod darwin;
mod guest;
mod net;
/// GUI application support (DRM/KMS dumb-buffer emulation's host-side presentation layer). See the
/// module's own doc comment for the full design, why its threading architecture is inverted from
/// `litebox_platform_windows_userland::presentation` (the reference implementation this was ported
/// from), and what remains before it can be wired into a real runner.
pub mod presentation;

use darwin::{
    KERN_NO_SPACE, KERN_SUCCESS, MAP_JIT, VM_FLAGS_FIXED, mach_task_self, mach_vm_allocate,
    mach_vm_deallocate, mach_vm_region_iter, ulock_wait, ulock_wake,
};

/// The host signal LiteBox reserves for interrupting a thread out of guest
/// execution. Darwin has no realtime signals, so this has to come out of the
/// small fixed set; `SIGUSR2` is the least likely to be wanted elsewhere in a
/// process that is already dedicating itself to hosting a sandbox.
const INTERRUPT_SIGNAL: libc::c_int = libc::SIGUSR2;

/// The userland macOS platform.
///
/// This implements the main [`litebox::platform::Provider`] trait, i.e.,
/// implements all platform traits.
pub struct MacOsUserland {
    /// Host mappings that already exist and must not be handed to a guest.
    reserved_pages: alloc::vec::Vec<core::ops::Range<usize>>,
    /// The boot session identifier, if
    /// [`Self::initialize_boot_specific_kdf_support`] has been run. It is stable
    /// across processes but changes on every boot.
    boot_id: OnceLock<alloc::vec::Vec<u8>>,
    /// Whether each of stdin/stdout/stderr is a terminal, sampled once at
    /// startup so the guest cannot observe a redirect mid-flight.
    stdio_is_tty: [bool; 3],
    /// The `utun` socket used for guest networking, if one was requested.
    tun: Option<std::os::fd::OwnedFd>,
}

impl core::fmt::Debug for MacOsUserland {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MacOsUserland").finish_non_exhaustive()
    }
}

impl MacOsUserland {
    /// Create a new userland-macOS platform for use in LiteBox.
    ///
    /// `tun_device_name` optionally names a `utun` interface (such as `"utun3"`)
    /// to connect guest networking to; networking is disabled when it is `None`.
    ///
    /// # Panics
    ///
    /// Panics if the requested `utun` device cannot be opened, or if the fault
    /// handlers that make guest-memory accesses fallible cannot be installed.
    pub fn new(tun_device_name: Option<&str>) -> &'static Self {
        install_fault_handlers();
        install_async_signal_handlers();

        let tun = tun_device_name.map(|name| {
            net::open_utun(name).unwrap_or_else(|e| panic!("failed to open {name}: {e}"))
        });

        let platform = Self {
            reserved_pages: read_memory_maps(),
            boot_id: OnceLock::new(),
            stdio_is_tty: [
                std::io::stdin().is_terminal(),
                std::io::stdout().is_terminal(),
                std::io::stderr().is_terminal(),
            ],
            tun,
        };

        // A platform must outlive every guest thread that can reach it, and
        // there is exactly one per process, so leaking is the cheapest way to
        // get a `'static` without reference counting on every access.
        alloc::boxed::Box::leak(alloc::boxed::Box::new(platform))
    }

    /// Populate the root key used by [`litebox::platform::DerivedKeyProvider`].
    ///
    /// The key is Darwin's boot session UUID: stable for every process in a boot
    /// and freshly generated by the kernel on the next one, which is exactly the
    /// "persistent across LiteBox invocations, reset by a true reboot" guarantee
    /// the trait describes.
    ///
    /// # Errors
    ///
    /// Returns the `sysctl` error if the boot session UUID cannot be read.
    pub fn initialize_boot_specific_kdf_support(&self) -> Result<(), std::io::Error> {
        if self.boot_id.get().is_some() {
            return Ok(());
        }
        let uuid = darwin::sysctl_string(c"kern.bootsessionuuid")?;
        // Ignore a concurrent initializer winning the race; both wrote the same
        // value.
        let _ = self.boot_id.set(uuid.into_bytes());
        Ok(())
    }

    /// The task parameters a runner should start the initial guest thread with.
    pub fn init_task(&self) -> litebox_common_linux::TaskParams {
        // TODO: these are synthetic, matching the other userland platforms.
        // Passing the host's real identity through is a separate decision about
        // what the guest is allowed to observe.
        litebox_common_linux::TaskParams {
            pid: 1000,
            ppid: 0,
            uid: 1000,
            gid: 1000,
            euid: 1000,
            egid: 1000,
        }
    }
}

impl litebox::platform::Provider for MacOsUserland {}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// Translate LiteBox permissions into Darwin `PROT_*` bits.
fn prot_flags(permissions: MemoryRegionPermissions) -> libc::c_int {
    let mut prot = libc::PROT_NONE;
    if permissions.contains(MemoryRegionPermissions::READ) {
        prot |= libc::PROT_READ;
    }
    if permissions.contains(MemoryRegionPermissions::WRITE) {
        prot |= libc::PROT_WRITE;
    }
    if permissions.contains(MemoryRegionPermissions::EXEC) {
        prot |= libc::PROT_EXEC;
    }
    prot
}

/// Whether a mapping with these permissions needs `MAP_JIT`.
///
/// Darwin refuses to make anonymous memory executable through the ordinary
/// path, and refuses to add `PROT_EXEC` to anything that was ever writable.
/// `MAP_JIT` is the supported way to get an executable mapping the process can
/// also write to, at the cost of an entitlement and of having to bracket writes
/// with [`jit_write_protect`].
fn needs_jit(permissions: MemoryRegionPermissions) -> bool {
    permissions.contains(MemoryRegionPermissions::EXEC)
}

/// Enable or disable write access to this thread's `MAP_JIT` mappings.
///
/// Darwin makes `MAP_JIT` memory writable *or* executable per thread, never
/// both at once. Pass `false` to write to a JIT mapping and `true` to execute
/// from it again. This must bracket every write LiteBox makes into guest code
/// pages -- loading segments, and applying the rewriter's patches.
///
/// # Safety
///
/// Toggling write protection off makes every `MAP_JIT` mapping in the process
/// writable and non-executable for this thread, so no code may be executed out
/// of a JIT mapping until protection is restored.
pub unsafe fn jit_write_protect(executable: bool) {
    // SAFETY: the call has no preconditions beyond the ones the caller of this
    // function is documented to uphold.
    unsafe { darwin::pthread_jit_write_protect_np(libc::c_int::from(executable)) }
}

/// Deliberately conservative. The user half of an Apple Silicon address space
/// is 47 bits wide, but the exact ceiling is a kernel implementation detail
/// (`MACH_VM_MAX_ADDRESS`) rather than a stable interface, so this stops a bit
/// below 2^46 -- 64 TiB of guest address space, comfortably inside any
/// plausible limit. Also used by [`read_memory_maps`] to exclude host
/// mappings the guest could never legally reach anyway, matching
/// [`litebox::platform::PageManagementProvider::TASK_ADDR_MAX`] below (which
/// can't reference this directly: it's an associated const on a trait
/// generic over `ALIGN`, not reachable from the free function).
///
/// A 1 GiB safety margin is subtracted from the round 64 TiB figure: a
/// mapping placed exactly at the ceiling (the top-down search's fast path
/// naturally does this once nothing above `TASK_ADDR_MAX` is tracked as
/// reserved -- see `read_memory_maps`) leaves a hint-based (non-`MAP_FIXED`)
/// hand-off to a real Darwin `mmap` call, e.g. `fork()`'s shared-memory
/// re-mapping, no room to round or pad the returned address even slightly
/// without exceeding `TASK_ADDR_MAX` and tripping `insert_mapping`'s own
/// `new_end <= TASK_ADDR_MAX` invariant (observed live: a `MAP_SHARED`
/// mapping placed at the exact top-down ceiling in the parent, then
/// hint-remapped during `Vmem::duplicate`, landed one page above it in the
/// child).
const TASK_ADDR_MAX: usize = 0x0000_4000_0000_0000 - 0x4000_0000;

impl<const ALIGN: usize> litebox::platform::PageManagementProvider<ALIGN> for MacOsUserland {
    /// The first 4 GiB of an arm64 Mach-O process is the `__PAGEZERO` segment:
    /// reserved, unmapped, and impossible to map over. Every guest address has
    /// to start above it, which is a real constraint on guest images -- an
    /// `ET_EXEC` binary linked at the customary `0x400000` cannot be loaded at
    /// its preferred address on this host.
    const TASK_ADDR_MIN: usize = 0x1_0000_0000;

    const TASK_ADDR_MAX: usize = TASK_ADDR_MAX;

    fn allocate_pages(
        &self,
        suggested_range: core::ops::Range<usize>,
        initial_permissions: MemoryRegionPermissions,
        _can_grow_down: bool,
        populate_pages_immediately: bool,
        fixed_address_behavior: FixedAddressBehavior,
    ) -> Result<Self::RawMutPointer<u8>, AllocationError> {
        if initial_permissions.contains(MemoryRegionPermissions::SHARED) {
            // A shared anonymous mapping needs a backing object on Darwin
            // (`MAP_SHARED|MAP_ANON` gives a per-process object, not one that
            // survives into a child), so it is not implemented rather than
            // silently wrong.
            return Err(AllocationError::OutOfMemory);
        }
        if !suggested_range.start.is_multiple_of(ALIGN)
            || !suggested_range.len().is_multiple_of(ALIGN)
        {
            return Err(AllocationError::Unaligned);
        }

        // `MAP_FIXED_NOREPLACE` has no Darwin equivalent, so claim the range
        // through the Mach VM API first: `mach_vm_allocate` with `VM_FLAGS_FIXED`
        // fails with `KERN_NO_SPACE` when any part of the range is already
        // mapped, which is exactly the semantics being emulated. The `mmap`
        // below then replaces the reservation in place.
        if fixed_address_behavior == FixedAddressBehavior::NoReplace {
            let mut addr = suggested_range.start as u64;
            // SAFETY: `mach_task_self()` names this process and the range is
            // page-aligned.
            let kr = unsafe {
                mach_vm_allocate(
                    mach_task_self(),
                    &raw mut addr,
                    suggested_range.len() as u64,
                    VM_FLAGS_FIXED,
                )
            };
            match kr {
                KERN_SUCCESS => {}
                KERN_NO_SPACE => return Err(AllocationError::AddressInUse),
                _ => return Err(AllocationError::OutOfMemory),
            }
            // The reservation above only exists to atomically prove the range
            // was free (a real Darwin `mmap(MAP_FIXED)` over a *live*
            // `mach_vm_allocate` object -- as opposed to over nothing, or over
            // another `mmap`-created mapping -- has been observed to fail with
            // `ENOMEM`). Release it immediately so the `mmap` below, a couple
            // of instructions later, creates the real mapping fresh rather
            // than replacing this reservation in place.
            //
            // SAFETY: this range was just reserved by the successful
            // `mach_vm_allocate` call above and nothing else in this
            // single-threaded sequence could have touched it yet.
            unsafe {
                mach_vm_deallocate(
                    mach_task_self(),
                    suggested_range.start as u64,
                    suggested_range.len() as u64,
                )
            };
        }

        let mut flags = libc::MAP_PRIVATE | libc::MAP_ANON;
        if fixed_address_behavior != FixedAddressBehavior::Hint {
            flags |= libc::MAP_FIXED;
        }
        if needs_jit(initial_permissions) {
            flags |= MAP_JIT;
        }

        // SAFETY: an anonymous mapping has no file backing to validate, and
        // `MAP_FIXED` only replaces a range the caller has told us it owns.
        let ptr = unsafe {
            libc::mmap(
                suggested_range.start as *mut libc::c_void,
                suggested_range.len(),
                prot_flags(initial_permissions),
                flags,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            // `EINVAL` from `mmap` here means a misaligned address or length,
            // since every other argument is fixed by this function. Everything
            // else -- `ENOMEM` included -- is reported as exhaustion.
            return Err(match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EINVAL) => AllocationError::Unaligned,
                _ => AllocationError::OutOfMemory,
            });
        }

        if populate_pages_immediately {
            // The closest Darwin has to `MAP_POPULATE`. It is advisory, so a
            // failure only costs later faults and is not worth reporting.
            // SAFETY: the range was just mapped.
            unsafe { libc::madvise(ptr, suggested_range.len(), libc::MADV_WILLNEED) };
        }

        Ok(UserMutPtr::from_ptr(ptr.cast::<u8>()))
    }

    unsafe fn deallocate_pages(
        &self,
        range: core::ops::Range<usize>,
    ) -> Result<(), DeallocationError> {
        if !range.start.is_multiple_of(ALIGN) || !range.len().is_multiple_of(ALIGN) {
            return Err(DeallocationError::Unaligned);
        }
        // SAFETY: the caller guarantees the range is no longer in use.
        let rc = unsafe { libc::munmap(range.start as *mut libc::c_void, range.len()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(DeallocationError::AlreadyUnallocated)
        }
    }

    unsafe fn update_permissions(
        &self,
        range: core::ops::Range<usize>,
        new_permissions: MemoryRegionPermissions,
    ) -> Result<(), PermissionUpdateError> {
        if !range.start.is_multiple_of(ALIGN) || !range.len().is_multiple_of(ALIGN) {
            return Err(PermissionUpdateError::Unaligned);
        }
        // NOTE: adding `PROT_EXEC` here succeeds only for a mapping that was
        // created with `MAP_JIT`. A region that has to become executable later
        // must therefore be allocated with `EXEC` in its initial permissions,
        // even if it is not executable yet.
        //
        // SAFETY: the caller guarantees the new permissions do not conflict with
        // any active use of the range.
        let rc = unsafe {
            libc::mprotect(
                range.start as *mut libc::c_void,
                range.len(),
                prot_flags(new_permissions),
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            // `EACCES` here means Darwin's W^X enforcement itself refused this
            // exact transition (see this function's own `NOTE` above) --
            // distinct from the range being unallocated, and a real, expected
            // outcome for a mapping that was ever writable gaining `EXEC`, not
            // a platform bug to work around.
            Err(match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EACCES) => PermissionUpdateError::Denied,
                _ => PermissionUpdateError::Unallocated,
            })
        }
    }

    fn reserved_pages(&self) -> impl Iterator<Item = &core::ops::Range<usize>> {
        self.reserved_pages.iter()
    }

    /// A `shm_open` file descriptor -- Darwin's nearest equivalent of Linux's
    /// `memfd_create`. Cheap to copy (a raw fd, not the memory itself);
    /// `close_shared_memory` (below, in this same `impl` block) closes it.
    type SharedMemoryHandle = libc::c_int;

    fn create_shared_memory(
        &self,
        size: usize,
    ) -> Result<Self::SharedMemoryHandle, SharedMemoryError> {
        // Darwin's `shm_open` has no `SHM_ANON`-style anonymous-object
        // shortcut (that is a FreeBSD extension libc does not expose here),
        // so this creates a real, uniquely-named object and unlinks it
        // immediately: the standard portable way to get an "anonymous"
        // shared-memory object from a `shm_open` that requires a name -- once
        // unlinked, the name can never collide with another process, and the
        // object itself is only released once every fd/mapping referencing
        // it is gone, exactly like the never-linked-in-the-first-place
        // `SHM_ANON` case on platforms that do have it.
        static COUNTER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `libc::getpid()` has no preconditions.
        let pid = unsafe { libc::getpid() };
        let name = std::ffi::CString::new(format!("/litebox-shm-{pid}-{id}"))
            .expect("no interior NUL in a formatted PID/counter name");

        // SAFETY: `name` is a valid NUL-terminated C string; `shm_open`'s
        // variadic `mode` argument undergoes C's default argument promotion
        // (any integer type narrower than `int` is promoted to `c_int`), so
        // it is passed as `libc::c_int` here to match what the callee reads,
        // not the `mode_t` (`u16`) the non-variadic call site's type suggests.
        let fd = unsafe {
            libc::shm_open(
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                0o600 as libc::c_int,
            )
        };
        if fd < 0 {
            return Err(SharedMemoryError::OutOfMemory);
        }
        // SAFETY: `name` is the same string `shm_open` just created.
        unsafe { libc::shm_unlink(name.as_ptr()) };
        // SAFETY: `fd` was just opened above and `size` is the caller-provided
        // byte length this object should hold.
        let rc = unsafe { libc::ftruncate(fd, size.cast_signed() as libc::off_t) };
        if rc != 0 {
            // SAFETY: `fd` is still open and owned by this call.
            unsafe { libc::close(fd) };
            return Err(SharedMemoryError::OutOfMemory);
        }
        Ok(fd)
    }

    fn map_shared_memory(
        &self,
        handle: Self::SharedMemoryHandle,
        suggested_range: core::ops::Range<usize>,
        initial_permissions: MemoryRegionPermissions,
        fixed_address_behavior: FixedAddressBehavior,
    ) -> Result<Self::RawMutPointer<u8>, SharedMemoryError> {
        if !suggested_range.start.is_multiple_of(ALIGN)
            || !suggested_range.len().is_multiple_of(ALIGN)
        {
            return Err(SharedMemoryError::Unaligned);
        }

        // Same `MAP_FIXED_NOREPLACE`-emulation pattern as `allocate_pages`:
        // Darwin has no such flag, so reserve the range through Mach first.
        if fixed_address_behavior == FixedAddressBehavior::NoReplace {
            let mut addr = suggested_range.start as u64;
            // SAFETY: `mach_task_self()` names this process and the range is
            // page-aligned.
            let kr = unsafe {
                mach_vm_allocate(
                    mach_task_self(),
                    &raw mut addr,
                    suggested_range.len() as u64,
                    VM_FLAGS_FIXED,
                )
            };
            match kr {
                KERN_SUCCESS => {}
                KERN_NO_SPACE => return Err(SharedMemoryError::AddressInUse),
                _ => return Err(SharedMemoryError::OutOfMemory),
            }
            // See `allocate_pages`'s matching deallocate-before-mmap comment:
            // the reservation only exists to atomically prove the range was
            // free, and a real `mmap(MAP_FIXED)` over a still-live
            // `mach_vm_allocate` object has been observed to fail with
            // `ENOMEM`.
            //
            // SAFETY: this range was just reserved by the successful
            // `mach_vm_allocate` call above and nothing else in this
            // single-threaded sequence could have touched it yet.
            unsafe {
                mach_vm_deallocate(
                    mach_task_self(),
                    suggested_range.start as u64,
                    suggested_range.len() as u64,
                )
            };
        }

        let mut flags = libc::MAP_SHARED;
        if fixed_address_behavior != FixedAddressBehavior::Hint {
            flags |= libc::MAP_FIXED;
        }

        // The caller (`litebox`'s `Vmem::insert_mapping`) always requests
        // `READ | WRITE | EXEC` here -- the widest permissions the mapping
        // could ever need -- then narrows to the real target via
        // `update_permissions` immediately after, to work around Windows'
        // `MapViewOfFile3` fixing a view's MAXIMUM protection at map time.
        // Darwin has no such ceiling (a later `mprotect` can freely widen or
        // narrow an `mmap`'d region), but it does enforce W^X: `mmap` with
        // `PROT_WRITE | PROT_EXEC` together fails outright without `MAP_JIT`
        // (see `needs_jit`'s doc comment above). Since Darwin doesn't need
        // the widest-permissions trick at all, request only `READ | WRITE`
        // for the initial mapping -- `update_permissions` below still narrows
        // (or, if a caller genuinely wants an executable shared mapping,
        // widens) to the real target, and never has to add `WRITE` back to
        // an already-executable page, so the W^X ceiling is never hit either
        // way.
        let map_permissions = initial_permissions - MemoryRegionPermissions::EXEC;

        // SAFETY: `handle` is a valid fd from `create_shared_memory`, and
        // `MAP_FIXED` only replaces a range the caller has told us it owns.
        let mut ptr = unsafe {
            libc::mmap(
                suggested_range.start as *mut libc::c_void,
                suggested_range.len(),
                prot_flags(map_permissions),
                flags,
                handle,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            let os_err = std::io::Error::last_os_error();
            return Err(match os_err.raw_os_error() {
                Some(libc::EINVAL) => SharedMemoryError::Unaligned,
                _ => SharedMemoryError::OutOfMemory,
            });
        }
        // A `Hint`-mode request (no `MAP_FIXED`) hands Darwin's own `mmap`
        // complete freedom to place the mapping anywhere; unlike Linux/
        // Windows, there is no way to pass Darwin an upper bound. If the
        // hint address (a real guest address from an existing mapping, e.g.
        // `Vmem::duplicate`'s "re-map the same MAP_SHARED handle at the
        // parent's own address" request) is contended, Darwin can fall back
        // to placing the mapping far outside this platform's own advertised
        // TASK_ADDR_MAX ceiling -- observed live, tripping the caller's own
        // `new_end <= TASK_ADDR_MAX` invariant. Retry once with NO hint
        // (`addr=0`) so Darwin picks freely from its normal low/preferred
        // region instead of wherever was contended near the original hint;
        // `Hint` mode never promised the hint address would be honored, so a
        // different final address here is a legal, ordinary outcome the
        // caller (`Vmem::insert_mapping`) already tracks via this function's
        // own return value, not a special case to plumb through.
        let out_of_range = |p: *mut libc::c_void| {
            let start = p as usize;
            let end = start.wrapping_add(suggested_range.len());
            start < <Self as litebox::platform::PageManagementProvider<ALIGN>>::TASK_ADDR_MIN
                || end > TASK_ADDR_MAX
        };
        if fixed_address_behavior == FixedAddressBehavior::Hint && out_of_range(ptr) {
            // SAFETY: this is exactly the mapping `mmap` just created above.
            unsafe { libc::munmap(ptr, suggested_range.len()) };
            // SAFETY: same as the original mapping attempt, minus the hint.
            ptr = unsafe {
                libc::mmap(
                    core::ptr::null_mut(),
                    suggested_range.len(),
                    prot_flags(map_permissions),
                    flags,
                    handle,
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(SharedMemoryError::OutOfMemory);
            }
        }
        if out_of_range(ptr) {
            // SAFETY: this is exactly the mapping `mmap` just created above.
            unsafe { libc::munmap(ptr, suggested_range.len()) };
            return Err(SharedMemoryError::OutOfMemory);
        }
        Ok(UserMutPtr::from_ptr(ptr.cast::<u8>()))
    }

    unsafe fn unmap_shared_memory(
        &self,
        range: core::ops::Range<usize>,
    ) -> Result<(), SharedMemoryError> {
        // SAFETY: the caller guarantees these pages are not in active use.
        let rc = unsafe { libc::munmap(range.start as *mut libc::c_void, range.len()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(SharedMemoryError::Unaligned)
        }
    }

    fn close_shared_memory(
        &self,
        handle: Self::SharedMemoryHandle,
    ) -> Result<(), SharedMemoryError> {
        // SAFETY: `handle` is a valid fd from `create_shared_memory`; closing
        // it releases this holder's reference. The kernel keeps the object
        // alive as long as any mapping of it (in this or another process)
        // still exists.
        unsafe { libc::close(handle) };
        Ok(())
    }
}

/// Enumerate the process's existing mappings so the guest is never offered an
/// address that dyld, the shared cache or the host heap already owns.
///
/// This is the Mach counterpart of the Windows platform's `VirtualQuery` walk.
///
/// Excludes anything at or above [`TASK_ADDR_MAX`]: real Darwin frameworks
/// (Metal, `libdispatch`'s `MALLOC_NANO` zone, and others) commonly place
/// large allocations extremely high in the address space -- observed up to
/// `0x600020000000`, itself already above this platform's own 64 TiB guest
/// ceiling -- and the guest can never legally be placed there anyway (every
/// guest address is bounded by `TASK_ADDR_MAX`), so tracking those mappings
/// serves no purpose. Worse, `Vmem::new`'s `last_range_value()` (the highest
/// TRACKED mapping, used to pick the top-down search's fast path) would
/// report one of these as the process's own "highest mapping" if they were
/// included, permanently forcing every top-down placement onto the slower
/// gap-search path instead -- which is exactly what was landing a
/// newly-loaded `ET_EXEC` interpreter far lower than intended.
fn read_memory_maps() -> alloc::vec::Vec<core::ops::Range<usize>> {
    mach_vm_region_iter()
        .filter(|range| range.start < TASK_ADDR_MAX)
        .map(|range| range.start..range.end.min(TASK_ADDR_MAX))
        .collect()
}

// ---------------------------------------------------------------------------
// Locking
// ---------------------------------------------------------------------------

impl litebox::platform::RawMutexProvider for MacOsUserland {
    type RawMutex = RawMutex;
}

/// A futex-equivalent built on Darwin's `ulock` compare-and-wait primitives.
pub struct RawMutex {
    inner: AtomicU32,
}

impl litebox::platform::RawMutex for RawMutex {
    const INIT: Self = Self {
        inner: AtomicU32::new(0),
    };

    fn underlying_atomic(&self) -> &AtomicU32 {
        &self.inner
    }

    fn wake_many(&self, n: usize) -> usize {
        // `ulock` can wake exactly one waiter or all of them, with nothing in
        // between, so anything above one wakes all. The trait permits this: a
        // wake is allowed to be spurious, and the return value is allowed to be
        // zero on a platform that cannot count what it woke -- which Darwin
        // cannot.
        ulock_wake(&self.inner, n > 1);
        0
    }

    fn block(&self, val: u32) -> Result<(), ImmediatelyWokenUp> {
        match self.block_inner(val, None) {
            Ok(UnblockedOrTimedOut::Unblocked) => Ok(()),
            Ok(UnblockedOrTimedOut::TimedOut) => {
                unreachable!("a wait with no deadline cannot time out")
            }
            Err(ImmediatelyWokenUp) => Err(ImmediatelyWokenUp),
        }
    }

    fn block_or_timeout(
        &self,
        val: u32,
        time: Duration,
    ) -> Result<UnblockedOrTimedOut, ImmediatelyWokenUp> {
        self.block_inner(val, Some(time))
    }
}

impl RawMutex {
    fn block_inner(
        &self,
        val: u32,
        timeout: Option<Duration>,
    ) -> Result<UnblockedOrTimedOut, ImmediatelyWokenUp> {
        match ulock_wait(&self.inner, val, timeout) {
            darwin::UlockWaitResult::Woken => Ok(UnblockedOrTimedOut::Unblocked),
            darwin::UlockWaitResult::TimedOut => Ok(UnblockedOrTimedOut::TimedOut),
            darwin::UlockWaitResult::ValueChanged => Err(ImmediatelyWokenUp),
        }
    }
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// A point on Darwin's monotonic clock, in nanoseconds.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant(u64);

impl litebox::platform::Instant for Instant {
    fn checked_duration_since(&self, earlier: &Self) -> Option<Duration> {
        self.0.checked_sub(earlier.0).map(Duration::from_nanos)
    }

    fn checked_add(&self, duration: Duration) -> Option<Self> {
        u64::try_from(duration.as_nanos())
            .ok()
            .and_then(|nanos| self.0.checked_add(nanos))
            .map(Self)
    }
}

/// A point on Darwin's wall clock, relative to the Unix epoch.
pub struct SystemTime {
    nanos_since_epoch: i128,
}

impl litebox::platform::SystemTime for SystemTime {
    const UNIX_EPOCH: Self = Self {
        nanos_since_epoch: 0,
    };

    fn duration_since(&self, earlier: &Self) -> Result<Duration, Duration> {
        let delta = self.nanos_since_epoch - earlier.nanos_since_epoch;
        let magnitude = Duration::from_nanos(delta.unsigned_abs().try_into().unwrap_or(u64::MAX));
        if delta < 0 {
            Err(magnitude)
        } else {
            Ok(magnitude)
        }
    }
}

impl litebox::platform::TimeProvider for MacOsUserland {
    type Instant = Instant;
    type SystemTime = SystemTime;

    fn now(&self) -> Self::Instant {
        // `CLOCK_MONOTONIC_RAW` is unaffected by NTP slewing and, like Linux's
        // `CLOCK_MONOTONIC`, does not advance while the machine is asleep.
        Instant(darwin::clock_gettime_nanos(libc::CLOCK_MONOTONIC_RAW))
    }

    fn current_time(&self) -> Self::SystemTime {
        SystemTime {
            nanos_since_epoch: i128::from(darwin::clock_gettime_nanos(libc::CLOCK_REALTIME)),
        }
    }
}

// ---------------------------------------------------------------------------
// Architecture-specific state
// ---------------------------------------------------------------------------

std::thread_local! {
    /// The guest's virtualized `TPIDR_EL0`.
    ///
    /// The host owns the hardware register as its own per-thread anchor, so the
    /// guest's thread pointer lives here instead. This is the slot the aarch64
    /// syscall rewriter's `MRS`/`MSR` gates read and write on the guest's behalf.
    static GUEST_TPIDR_EL0: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

impl litebox::platform::ArchSpecificProvider for MacOsUserland {
    fn get_arch_specific_register(
        &self,
        reg: &litebox::platform::ArchSpecificRegister,
    ) -> Result<usize, litebox::platform::ArchSpecificError> {
        match reg {
            litebox::platform::ArchSpecificRegister::TpidrEl0 => Ok(GUEST_TPIDR_EL0.get()),
            _ => Err(litebox::platform::ArchSpecificError::RegisterUnsupported),
        }
    }

    fn set_arch_specific_register(
        &self,
        reg: &litebox::platform::ArchSpecificRegister,
        val: usize,
    ) -> Result<(), litebox::platform::ArchSpecificError> {
        match reg {
            litebox::platform::ArchSpecificRegister::TpidrEl0 => {
                // No range check is needed: the value never reaches a hardware
                // register, so an invalid one can only fault the guest that
                // dereferences it.
                GUEST_TPIDR_EL0.set(val);
                Ok(())
            }
            _ => Err(litebox::platform::ArchSpecificError::RegisterUnsupported),
        }
    }
}

// ---------------------------------------------------------------------------
// Pointers
// ---------------------------------------------------------------------------

type UserConstPtr<T> = litebox::platform::common_providers::userspace_pointers::UserConstPtr<
    litebox::platform::common_providers::userspace_pointers::NoValidation,
    T,
>;
type UserMutPtr<T> = litebox::platform::common_providers::userspace_pointers::UserMutPtr<
    litebox::platform::common_providers::userspace_pointers::NoValidation,
    T,
>;

impl litebox::platform::RawPointerProvider for MacOsUserland {
    type RawConstPointer<T: FromBytes> = UserConstPtr<T>;
    type RawMutPointer<T: FromBytes + IntoBytes> = UserMutPtr<T>;
}

// ---------------------------------------------------------------------------
// Standard I/O
// ---------------------------------------------------------------------------

impl litebox::platform::StdioProvider for MacOsUserland {
    fn read_from_stdin(&self, buf: &mut [u8]) -> Result<usize, litebox::platform::StdioReadError> {
        // SAFETY: `buf` is a valid writable slice of the length passed.
        let n = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
            )
        };
        usize::try_from(n).map_err(|_| litebox::platform::StdioReadError::Closed)
    }

    fn write_to(
        &self,
        stream: litebox::platform::StdioOutStream,
        buf: &[u8],
    ) -> Result<usize, litebox::platform::StdioWriteError> {
        let fd = match stream {
            litebox::platform::StdioOutStream::Stdout => libc::STDOUT_FILENO,
            litebox::platform::StdioOutStream::Stderr => libc::STDERR_FILENO,
        };
        // SAFETY: `buf` is a valid readable slice of the length passed.
        let n = unsafe { libc::write(fd, buf.as_ptr().cast::<libc::c_void>(), buf.len()) };
        usize::try_from(n).map_err(|_| litebox::platform::StdioWriteError::Closed)
    }

    fn is_a_tty(&self, stream: litebox::platform::StdioStream) -> bool {
        self.stdio_is_tty[stream as usize]
    }

    fn stdin_ready(&self) -> bool {
        // A real `poll(2)` on the actual inherited stdin fd with a zero timeout: the host
        // kernel already implements this readiness query directly against the real fd, no
        // emulation required -- see `litebox_platform_linux_userland`'s identical rationale for
        // its own `stdin_ready`.
        let mut pfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is a single valid `pollfd`, `nfds` matches, and a zero timeout makes
        // this call non-blocking.
        let ret = unsafe { libc::poll(&raw mut pfd, 1, 0) };
        ret > 0
    }
}

// ---------------------------------------------------------------------------
// System information
// ---------------------------------------------------------------------------

impl litebox::platform::SystemInfoProvider for MacOsUserland {
    fn get_syscall_entry_point(&self) -> usize {
        guest::syscall_callback as *const () as usize
    }

    fn get_vdso_address(&self) -> Option<usize> {
        // A Linux guest's vDSO would have to be a LiteBox-provided image; the
        // host's own `commpage` is not one. Reporting `None` means the guest
        // falls back to real syscalls, which is what the shim wants anyway --
        // but it also means a guest signal handler needs its own `sa_restorer`,
        // because the kernel's fallback trampoline lives in the vDSO.
        None
    }
}

// ---------------------------------------------------------------------------
// Fork-child verification
// ---------------------------------------------------------------------------

// Guest entry (the host<->guest context switch) is not implemented yet -- see
// docs/macos.md's "Remaining work" -- so there is no real fork()ed guest
// execution for this platform to verify. The default (no-op) trait methods
// are correct here, matching litebox_platform_linux_userland's own empty
// impl; only litebox_platform_windows_userland's real relocation-verification
// machinery needs to override these.
impl litebox::platform::ForkChildVerificationProvider for MacOsUserland {}

// ---------------------------------------------------------------------------
// Thread-local storage
// ---------------------------------------------------------------------------

std::thread_local! {
    static PLATFORM_TLS: core::cell::Cell<*mut ()> =
        const { core::cell::Cell::new(core::ptr::null_mut()) };
}

// SAFETY: the pointer returned is exactly the one most recently stored for this
// thread, and a thread that has stored nothing reads back the null the cell was
// initialized with.
unsafe impl litebox::platform::ThreadLocalStorageProvider for MacOsUserland {
    fn get_thread_local_storage() -> *mut () {
        PLATFORM_TLS.get()
    }

    unsafe fn replace_thread_local_storage(value: *mut ()) -> *mut () {
        PLATFORM_TLS.replace(value)
    }
}

// ---------------------------------------------------------------------------
// Randomness and derived keys
// ---------------------------------------------------------------------------

impl litebox::platform::CrngProvider for MacOsUserland {
    fn fill_bytes_crng(&self, buf: &mut [u8]) {
        // `arc4random_buf` is the platform's own CSPRNG: it cannot fail, cannot
        // block, and the kernel reseeds it across `fork` and VM snapshots. That
        // pass-through is precisely what the trait asks for.
        //
        // SAFETY: `buf` is a valid writable slice of the length passed.
        unsafe { libc::arc4random_buf(buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
    }
}

impl litebox::platform::DerivedKeyProvider for MacOsUserland {
    fn derive_key<E>(
        &self,
        shim_kdf: Option<fn(&[u8], litebox::platform::KDFParams) -> Result<(), E>>,
        params: litebox::platform::KDFParams,
    ) -> Result<(), litebox::platform::DerivedKeyError<E>> {
        let Some(boot_id) = self.boot_id.get() else {
            return Err(litebox::platform::DerivedKeyError::UnsupportedRebootPersistentKey);
        };
        let Some(shim_kdf) = shim_kdf else {
            // Darwin exposes no KDF of its own that is rooted in a device
            // secret, so a shim that brings none cannot be served here.
            return Err(litebox::platform::DerivedKeyError::ShimKDFRequired);
        };
        // The shim shares this platform's trust boundary, so the root key can be
        // handed to it directly rather than pre-hashed.
        Ok(shim_kdf(boot_id, params)?)
    }
}

// ---------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------

/// Asynchronous host signals observed since the guest last drained them.
///
/// Bit `n - 1` corresponds to *guest* (Linux) signal number `n`, matching
/// `SigSet`'s encoding -- NOT the raw host signal number `async_signal_handler`
/// receives. Darwin and Linux agree on the numbering for `SIGINT`/`SIGALRM`/
/// `SIGVTALRM`/`SIGPROF`, but not for `SIGUSR1` (10 on Linux, 30 on Darwin --
/// see `unix/bsd/mod.rs` in the vendored `libc` crate), so the handler below
/// translates the host number it actually received to the guest number
/// before setting a bit, rather than assuming they're the same value.
static PENDING_SIGNALS: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn async_signal_handler(signum: libc::c_int) {
    // Translate the HOST signal number this handler actually received to the
    // GUEST (Linux) number `PENDING_SIGNALS`/`take_pending_signals` use --
    // see this static's own doc comment for why they can differ. Every
    // signal `install_async_signal_handlers` installs this handler for must
    // have an arm here.
    use litebox_common_linux::signal::Signal;
    let guest_signal = match signum {
        libc::SIGINT => Signal::SIGINT,
        libc::SIGALRM => Signal::SIGALRM,
        libc::SIGVTALRM => Signal::SIGVTALRM,
        libc::SIGPROF => Signal::SIGPROF,
        libc::SIGUSR1 => Signal::SIGUSR1,
        _ => return,
    };
    if let Ok(bit) = u32::try_from(guest_signal.as_i32() - 1) {
        PENDING_SIGNALS.fetch_or(1u64 << bit, Ordering::Relaxed);
    }
}

/// The interrupt signal exists only to kick a thread out of a blocking call;
/// the handler itself has nothing to do.
unsafe extern "C" fn interrupt_signal_handler(_signum: libc::c_int) {}

fn install_async_signal_handlers() {
    // SIGALRM/SIGVTALRM/SIGPROF/SIGUSR1 are the signals `create_timer` (below)
    // can be asked to deliver. SIGUSR2 is deliberately excluded: it is
    // reserved as `INTERRUPT_SIGNAL` below, for kicking a host thread out of
    // a blocking call, not for guest-requested timer delivery.
    for signum in [
        libc::SIGINT,
        libc::SIGALRM,
        libc::SIGVTALRM,
        libc::SIGPROF,
        libc::SIGUSR1,
    ] {
        darwin::install_handler(signum, async_signal_handler as *const () as usize, false);
    }
    // `SA_RESTART` is deliberately absent: interrupting a blocking call is the
    // entire purpose of this signal.
    darwin::install_handler(
        INTERRUPT_SIGNAL,
        interrupt_signal_handler as *const () as usize,
        false,
    );
}

impl litebox::platform::SignalProvider for MacOsUserland {
    type Signal = litebox_common_linux::signal::Signal;

    fn take_pending_signals(&self, mut f: impl FnMut(Self::Signal)) {
        let mut pending = PENDING_SIGNALS.swap(0, Ordering::Relaxed);
        while pending != 0 {
            let bit = pending.trailing_zeros();
            pending &= !(1u64 << bit);
            // The bit already encodes a GUEST signal number -- see
            // `PENDING_SIGNALS`'s own doc comment -- so this is a plain
            // decode, not a host-to-guest translation.
            if let Ok(signal) =
                litebox_common_linux::signal::Signal::try_from(bit.cast_signed() + 1)
            {
                f(signal);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Timers
// ---------------------------------------------------------------------------

/// A one-shot timer.
///
/// Darwin has neither POSIX `timer_create` nor more than one `setitimer` per
/// process, so each timer owns a thread that sleeps until its deadline and then
/// raises the signal. The thread is parked on a condition variable when no
/// deadline is armed, so an idle timer costs nothing but its stack.
pub struct TimerHandle {
    state: Arc<TimerState>,
}

struct TimerState {
    /// The deadline, or `None` when disarmed. `Condvar` wakes the timer thread
    /// whenever this changes.
    deadline: Mutex<TimerCommand>,
    changed: Condvar,
    signal: libc::c_int,
    /// The thread that created this timer, matching real POSIX `alarm`/
    /// `setitimer` delivering to the thread that armed them. Raising `signal`
    /// alone only sets a bit in `PENDING_SIGNALS` -- a thread already parked
    /// in a blocking wait (e.g. `nanosleep`) never re-checks that bit on its
    /// own, so this thread also has to be woken via the same
    /// `ThreadHandle::interrupt` mechanism litebox's own cross-thread wakeups
    /// use, or the signal is recorded but never observed until the blocked
    /// call's own timeout independently expires.
    owner: ThreadHandle,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TimerCommand {
    Disarmed,
    ArmedFor(std::time::Instant),
    Deleted,
}

impl litebox::platform::TimerProvider for MacOsUserland {
    type TimerHandle = TimerHandle;
    type Signal = litebox_common_linux::signal::Signal;

    fn create_timer(
        &self,
        signal: Self::Signal,
    ) -> Result<Self::TimerHandle, litebox::platform::TimerCreationError> {
        // Only the signals `install_async_signal_handlers` installed a host
        // handler for can be raised this way.
        use litebox_common_linux::signal::Signal;
        let host_signal = match signal {
            Signal::SIGALRM => libc::SIGALRM,
            Signal::SIGVTALRM => libc::SIGVTALRM,
            Signal::SIGPROF => libc::SIGPROF,
            Signal::SIGUSR1 => libc::SIGUSR1,
            _ => return Err(litebox::platform::TimerCreationError::Unsupported),
        };
        let state = Arc::new(TimerState {
            deadline: Mutex::new(TimerCommand::Disarmed),
            changed: Condvar::new(),
            signal: host_signal,
            owner: ThreadHandle::current(),
        });
        let thread_state = Arc::clone(&state);
        std::thread::Builder::new()
            .name("litebox-timer".into())
            .spawn(move || timer_thread(&thread_state))
            .map_err(|_| litebox::platform::TimerCreationError::Unsupported)?;
        Ok(TimerHandle { state })
    }
}

fn timer_thread(state: &TimerState) {
    let mut command = state.deadline.lock().unwrap();
    loop {
        match *command {
            TimerCommand::Deleted => return,
            TimerCommand::Disarmed => {
                command = state.changed.wait(command).unwrap();
            }
            TimerCommand::ArmedFor(deadline) => {
                let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now())
                else {
                    // Fire, then disarm -- these timers are one-shot.
                    *command = TimerCommand::Disarmed;
                    // SAFETY: raising a signal at the process level is always
                    // well-defined; the handler is already installed.
                    unsafe { libc::raise(state.signal) };
                    // Raising alone only sets a bit in `PENDING_SIGNALS`; the
                    // owning thread must also be woken if it's currently
                    // parked in a blocking wait, or it won't observe the
                    // pending signal until that wait's own timeout expires on
                    // its own (see `TimerState::owner`'s doc comment).
                    state.owner.interrupt();
                    continue;
                };
                let (next, _) = state.changed.wait_timeout(command, remaining).unwrap();
                command = next;
            }
        }
    }
}

impl litebox::platform::TimerHandle for TimerHandle {
    fn set_timer(&self, duration: Duration) {
        let mut command = self.state.deadline.lock().unwrap();
        *command = if duration.is_zero() {
            TimerCommand::Disarmed
        } else {
            TimerCommand::ArmedFor(std::time::Instant::now() + duration)
        };
        drop(command);
        self.state.changed.notify_all();
    }

    fn delete_timer(self) {
        let mut command = self.state.deadline.lock().unwrap();
        *command = TimerCommand::Deleted;
        drop(command);
        self.state.changed.notify_all();
    }
}

// ---------------------------------------------------------------------------
// Threads
// ---------------------------------------------------------------------------

/// A `pthread_t` is an opaque pointer on Darwin, so it needs an explicit
/// `Send`/`Sync` witness to travel between threads.
#[derive(Clone, Copy)]
struct ThreadId(libc::pthread_t);

// SAFETY: the identifier is only ever handed back to `pthread_kill`, which is
// itself thread-safe, and [`ThreadHandle`] clears it before the thread it names
// can exit, so it is never used to signal a dead thread.
unsafe impl Send for ThreadId {}
// SAFETY: see the `Send` witness above.
unsafe impl Sync for ThreadId {}

/// A handle to a LiteBox-managed thread, used to interrupt it.
pub struct ThreadHandle(Arc<Mutex<Option<ThreadId>>>);

impl Clone for ThreadHandle {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

std::thread_local! {
    static CURRENT_THREAD: core::cell::RefCell<Option<ThreadHandle>> =
        const { core::cell::RefCell::new(None) };
}

impl ThreadHandle {
    /// Runs `f` with a handle registered for the current thread, so that
    /// [`litebox::platform::ThreadProvider::current_thread`] works inside it.
    fn run_with_handle<R>(f: impl FnOnce() -> R) -> R {
        // SAFETY: `pthread_self` has no preconditions.
        let handle = ThreadHandle(Arc::new(Mutex::new(Some(ThreadId(unsafe {
            libc::pthread_self()
        })))));
        CURRENT_THREAD.with_borrow_mut(|current| {
            assert!(
                current.is_none(),
                "nested run_with_handle calls are not supported"
            );
            *current = Some(handle);
        });
        let _guard = litebox::utils::defer(|| {
            let current = CURRENT_THREAD.take().expect("handle registered above");
            // Clearing before the thread exits is what makes signalling a stale
            // `pthread_t` impossible.
            *current.0.lock().unwrap() = None;
        });
        f()
    }

    fn current() -> Self {
        CURRENT_THREAD.with_borrow(|thread| {
            thread
                .clone()
                .expect("current_thread called outside of a LiteBox thread")
        })
    }

    fn interrupt(&self) {
        if let Some(thread) = *self.0.lock().unwrap() {
            // SAFETY: the identifier is live for as long as this lock is held.
            unsafe { libc::pthread_kill(thread.0, INTERRUPT_SIGNAL) };
        }
    }
}

impl litebox::platform::ThreadProvider for MacOsUserland {
    type ExecutionContext = litebox_common_linux::PtRegs;
    type ThreadSpawnError = std::io::Error;
    type ThreadHandle = ThreadHandle;

    unsafe fn spawn_thread(
        &self,
        ctx: &litebox_common_linux::PtRegs,
        init_thread: alloc::boxed::Box<
            dyn litebox::shim::InitThread<ExecutionContext = litebox_common_linux::PtRegs>,
        >,
    ) -> Result<(), Self::ThreadSpawnError> {
        let mut ctx = ctx.clone();
        std::thread::Builder::new()
            .name("litebox-guest".into())
            .spawn(move || {
                // Let the shim set up its per-thread state before the new thread
                // reaches guest code.
                let shim = init_thread.init();
                ThreadHandle::run_with_handle(|| guest::run_thread(shim.as_ref(), &mut ctx));
            })?;
        Ok(())
    }

    fn current_thread(&self) -> Self::ThreadHandle {
        ThreadHandle::current()
    }

    fn interrupt_thread(&self, thread: &Self::ThreadHandle) {
        thread.interrupt();
    }

    #[cfg(debug_assertions)]
    fn run_test_thread<R>(f: impl FnOnce() -> R) -> R {
        ThreadHandle::run_with_handle(f)
    }
}

// ---------------------------------------------------------------------------
// Networking
// ---------------------------------------------------------------------------

impl litebox::platform::IPInterfaceProvider for MacOsUserland {
    fn send_ip_packet(&self, packet: &[u8]) -> Result<(), litebox::platform::SendError> {
        // Without a `utun` device there is nowhere for the packet to go.
        // `SendError` is `#[non_exhaustive]` with no variants, so silently
        // dropping is the only representable outcome -- and it matches what a
        // guest with no configured interface should observe.
        if let Some(tun) = self.tun.as_ref() {
            net::write_packet(tun, packet);
        }
        Ok(())
    }

    fn receive_ip_packet(
        &self,
        packet: &mut [u8],
    ) -> Result<usize, litebox::platform::ReceiveError> {
        let Some(tun) = self.tun.as_ref() else {
            return Err(litebox::platform::ReceiveError::WouldBlock);
        };
        net::read_packet(tun, packet).ok_or(litebox::platform::ReceiveError::WouldBlock)
    }
}

// ---------------------------------------------------------------------------
// Faults
// ---------------------------------------------------------------------------

/// Install the handlers that make LiteBox's accesses to guest memory fallible.
///
/// Without these, a `UserConstPtr` read of an unmapped guest address takes the
/// process down instead of returning `None`; see
/// [`litebox::platform::common_providers::userspace_pointers`].
fn install_fault_handlers() {
    for signum in [libc::SIGSEGV, libc::SIGBUS] {
        darwin::install_handler(signum, fault_handler as *const () as usize, true);
    }
}

unsafe extern "C" fn fault_handler(
    signum: libc::c_int,
    _info: *mut libc::siginfo_t,
    ucontext: *mut libc::c_void,
) {
    // SAFETY: the kernel hands a `ucontext_t` to an `SA_SIGINFO` handler, and
    // its `uc_mcontext` points at a `_STRUCT_MCONTEXT64` on this architecture.
    let machine_context = unsafe {
        let ucontext = ucontext.cast::<libc::ucontext_t>();
        if ucontext.is_null() {
            darwin::reraise_fatally(signum);
            return;
        }
        (*ucontext).uc_mcontext.cast::<darwin::McontextPrefix64>()
    };
    if machine_context.is_null() {
        darwin::reraise_fatally(signum);
        return;
    }

    // SAFETY: checked non-null just above.
    let pc = unsafe { (*machine_context).thread_state.pc };
    if let Some(recovery) = litebox::mm::exception_table::search_exception_tables(pc.trunc()) {
        // A LiteBox access to guest memory faulted and the exception table says
        // where to continue; redirecting the program counter is what turns the
        // fault into a `None` return.
        //
        // SAFETY: checked non-null just above.
        unsafe { (*machine_context).thread_state.pc = recovery as u64 };
        return;
    }

    // Not a recoverable LiteBox access. Restore the default disposition and
    // return so the faulting instruction re-executes and takes the process down
    // exactly as it would have without this handler installed.
    darwin::reraise_fatally(signum);
}

/// Page faults are serviced by the host kernel, so LiteBox never handles one
/// itself here. Provided to satisfy the trait bound on `PageManager`.
impl litebox::mm::linux::VmemPageFaultHandler for MacOsUserland {
    unsafe fn handle_page_fault(
        &self,
        _fault_addr: usize,
        _flags: litebox::mm::linux::VmFlags,
        _error_code: u64,
    ) -> Result<(), litebox::mm::linux::PageFaultError> {
        unreachable!("host kernel handles page faults for macOS userland")
    }

    fn access_error(_error_code: u64, _flags: litebox::mm::linux::VmFlags) -> bool {
        unreachable!("host kernel handles page faults for macOS userland")
    }
}
