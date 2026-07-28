// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Page-management related types and traits

use crate::platform::{RawConstPointer as _, RawMutPointer as _};

use super::RawPointerProvider;
use core::ops::Range;
use thiserror::Error;

bitflags::bitflags! {
    /// Permissions for a memory region
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct MemoryRegionPermissions: u8 {
        /// Readable
        const READ = 1 << 0;
        /// Writable
        const WRITE = 1 << 1;
        /// Executable
        const EXEC = 1 << 2;
        /// Sharable between processes
        const SHARED = 1 << 3;
    }
}

/// A provider for managing memory pages
///
/// NOTE: Due to insufficient support for associated constants in current Stable Rust, we have
/// `ALIGN` as a parameter. In the future, this may be changed to an associated constant, since each
/// platform has only one canonical alignment.
pub trait PageManagementProvider<const ALIGN: usize>: RawPointerProvider {
    /// The lower bound (inclusive) for virtual addresses that can be allocated for task memory.
    ///
    /// Note it must be aligned to `ALIGN`.
    const TASK_ADDR_MIN: usize;
    /// The upper bound (exclusive) for virtual addresses that can be allocated for task memory.
    ///
    /// Note it must be aligned to `ALIGN`.
    const TASK_ADDR_MAX: usize;

    /// Allocates new memory pages at the specified `suggested_range` with the given `initial_permissions`.
    ///
    /// # Parameters
    ///
    /// - `suggested_range`: A suggested address range for the allocation.
    /// - `initial_permissions`: The permissions to apply to the allocated memory region.
    /// - `can_grow_down`: If `true`, the region is allowed to grow downward (towards zero) upon
    ///   a page fault.
    /// - `populate_pages_immediately`: If `true`, the pages are populated immediately; otherwise,
    ///   they are populated lazily.
    /// - `fixed_address_behavior`: Specifies the required semantics of `suggested_range`.
    ///
    /// # Returns
    ///
    /// On success, returns a raw mutable pointer to the start of the allocated memory region.
    ///
    /// # Errors
    ///
    /// Returns an [`AllocationError`] if the allocation fails.
    fn allocate_pages(
        &self,
        suggested_range: Range<usize>,
        initial_permissions: MemoryRegionPermissions,
        can_grow_down: bool,
        populate_pages_immediately: bool,
        fixed_address_behavior: FixedAddressBehavior,
    ) -> Result<Self::RawMutPointer<u8>, AllocationError>;

    /// De-allocated all pages in the given `range`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that these pages are not in active use.
    unsafe fn deallocate_pages(&self, range: Range<usize>) -> Result<(), DeallocationError>;

    /// Remap pages from `old_range` to `new_range`.
    ///
    /// ## Returns
    ///
    /// On success it returns a pointer to the new virtual memory area.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it is safe to move the `old_range` (i.e., these pages are not in
    /// active use).
    ///
    /// The `new_range` must be larger than `old_range`, and must not overlap with `old_range`.
    ///
    /// Both ranges must be aligned to `ALIGN`.
    unsafe fn remap_pages(
        &self,
        old_range: Range<usize>,
        new_range: Range<usize>,
        permissions: MemoryRegionPermissions,
    ) -> Result<Self::RawMutPointer<u8>, RemapError> {
        debug_assert!(old_range.start.is_multiple_of(ALIGN));
        debug_assert!(new_range.start.is_multiple_of(ALIGN));
        debug_assert!(old_range.len().is_multiple_of(ALIGN));
        debug_assert!(new_range.len().is_multiple_of(ALIGN));
        debug_assert!(new_range.len() > old_range.len());
        debug_assert!(old_range.start.max(new_range.start) >= old_range.end.min(new_range.end));
        // Default implementation: allocate new pages, copy data, deallocate old pages
        let temp_permissions = permissions | MemoryRegionPermissions::WRITE;
        let new_ptr = self
            .allocate_pages(
                new_range.clone(),
                temp_permissions,
                false,
                true,
                FixedAddressBehavior::NoReplace,
            )
            .map_err(|e| match e {
                AllocationError::OutOfMemory => RemapError::OutOfMemory,
                AllocationError::AddressInUse | AllocationError::AddressInUseByPlatform => {
                    RemapError::AlreadyAllocated
                }
                AllocationError::Unaligned
                | AllocationError::BelowMinAddress
                | AllocationError::AboveMaxAddress
                | AllocationError::AddressPartiallyInUse => unreachable!(),
            })?;

        // Copy memory from old range to new range
        if !permissions.contains(MemoryRegionPermissions::READ) {
            (unsafe {
                self.update_permissions(
                    old_range.clone(),
                    permissions | MemoryRegionPermissions::READ,
                )
            })
            .expect("failed to update permissions on old range for copying");
        }
        // Copy in chunks of ALIGN bytes to handle very large memory regions
        let total_len = old_range.len();
        let mut offset = 0;
        while offset < total_len {
            let chunk_len = (total_len - offset).min(ALIGN);
            let old_ptr =
                <Self as RawPointerProvider>::RawConstPointer::from_usize(old_range.start + offset);
            new_ptr
                .write_slice_at_offset(
                    isize::try_from(offset).unwrap(),
                    &old_ptr.to_owned_slice(chunk_len).unwrap(),
                )
                .unwrap();
            offset += ALIGN;
        }

        if temp_permissions != permissions {
            (unsafe { self.update_permissions(new_range.clone(), permissions) })
                .expect("failed to restore permissions on new range");
        }

        (unsafe { self.deallocate_pages(old_range) }).expect("failed to deallocate old range");

        Ok(new_ptr)
    }

    /// Update the permissions on pages in `range` to `new_permissions`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the permissions do not conflict with any currently active usage
    /// of these pages.
    unsafe fn update_permissions(
        &self,
        range: Range<usize>,
        new_permissions: MemoryRegionPermissions,
    ) -> Result<(), PermissionUpdateError>;

    /// Return reserved pages that are not available for allocation.
    ///
    /// Note that the returned ranges should be `ALIGN`-aligned.
    fn reserved_pages(&self) -> impl Iterator<Item = &Range<usize>>;

    /// Attempt to allocate pages with copy-on-write semantics backed by static data.
    ///
    /// This method allows platforms that support it to create CoW mappings instead of performing
    /// expensive page-by-page memory copies. This is particularly useful when mapping pre-loaded
    /// file data that was mmap'd by the host.
    ///
    /// The default implementation returns unsupported CoW. Platforms that DO support COW should
    /// override this method to unlock better performance.
    #[expect(unused_variables, reason = "default body, non-underscored param names")]
    fn try_allocate_cow_pages(
        &self,
        suggested_start: usize,
        source_data: &'static [u8],
        permissions: MemoryRegionPermissions,
        fixed_address_behavior: FixedAddressBehavior,
    ) -> Result<Self::RawMutPointer<u8>, CowAllocationError> {
        Err(CowAllocationError::UnsupportedByPlatform)
    }

    /// An opaque handle to a platform-level shared-memory object, e.g. a Windows file-mapping
    /// `HANDLE` or a Linux `memfd_create` file descriptor. Cheap to copy (a raw handle/fd, not
    /// the memory itself); [`Self::close_shared_memory`] releases the underlying object.
    ///
    /// Platforms that don't support real cross-process shared memory (e.g. bare-metal backends
    /// with no host-OS shared-memory primitive to lean on) should set this to `()`: their
    /// [`Self::create_shared_memory`] returns [`SharedMemoryError::UnsupportedByPlatform`]
    /// unconditionally (the default body), so this type is never actually constructed.
    ///
    /// `Send + Sync` because it lives inside `Vmem`'s tracked mappings, which are shared,
    /// cross-thread state (e.g. `litebox_shim_linux`'s per-process `PageManager`) -- a shared-
    /// memory handle is fundamentally just an opaque OS-level identifier (a Windows `HANDLE` or
    /// a Linux fd number) with no thread-affinity, so this is not an additional runtime
    /// requirement on real implementations, only a bound the compiler needs stated explicitly.
    type SharedMemoryHandle: Copy + Eq + Send + Sync + core::fmt::Debug;

    /// Creates a new platform-level shared-memory object of `size` bytes (not yet mapped into
    /// any address range) and returns a handle to it.
    ///
    /// This is the primitive that makes `MAP_SHARED` mappings genuinely shared across `fork()`:
    /// unlike [`Self::allocate_pages`] (whose memory belongs to exactly one virtual-address
    /// range, on this single-host-process design not visible to a fork()ed child at all without
    /// an eager copy), the SAME handle returned here can be mapped into more than one address
    /// range (via [`Self::map_shared_memory`], including
    /// after a fork, at a possibly different address per [`crate::mm::AddressRelocations`]) while
    /// all mappings observe the same underlying physical pages -- a write through one mapping is
    /// visible through the others.
    ///
    /// The default implementation returns [`SharedMemoryError::UnsupportedByPlatform`].
    /// Platforms that DO support real OS-level shared memory (Windows via `CreateFileMappingW`,
    /// Linux userland via `memfd_create`) should override this to unlock real `MAP_SHARED`
    /// semantics; without it, `MAP_SHARED` mappings degrade to copy-on-fork (see
    /// `mmap-map-shared-real-cross-process-semantics`'s original gap).
    #[expect(unused_variables, reason = "default body, non-underscored param names")]
    fn create_shared_memory(
        &self,
        size: usize,
    ) -> Result<Self::SharedMemoryHandle, SharedMemoryError> {
        Err(SharedMemoryError::UnsupportedByPlatform)
    }

    /// Maps `handle` (from [`Self::create_shared_memory`]) into the address space at
    /// `suggested_range`, with the given semantics -- the same request shape as
    /// [`Self::allocate_pages`], since from the caller's perspective this is just another way to
    /// bring pages into the address space, only backed by a shared object instead of fresh
    /// anonymous memory.
    ///
    /// The default implementation returns [`SharedMemoryError::UnsupportedByPlatform`]; see
    /// [`Self::create_shared_memory`]'s doc comment.
    #[expect(unused_variables, reason = "default body, non-underscored param names")]
    fn map_shared_memory(
        &self,
        handle: Self::SharedMemoryHandle,
        suggested_range: Range<usize>,
        initial_permissions: MemoryRegionPermissions,
        fixed_address_behavior: FixedAddressBehavior,
    ) -> Result<Self::RawMutPointer<u8>, SharedMemoryError> {
        Err(SharedMemoryError::UnsupportedByPlatform)
    }

    /// Unmaps `range` (previously mapped via [`Self::map_shared_memory`]) from the address space,
    /// WITHOUT releasing the underlying shared-memory object -- other mappings of the same handle
    /// (e.g. in the parent, after a fork) remain valid and continue to observe the same memory.
    ///
    /// # Safety
    ///
    /// The caller must ensure that these pages are not in active use.
    ///
    /// The default implementation returns [`SharedMemoryError::UnsupportedByPlatform`]; see
    /// [`Self::create_shared_memory`]'s doc comment.
    #[expect(unused_variables, reason = "default body, non-underscored param names")]
    unsafe fn unmap_shared_memory(&self, range: Range<usize>) -> Result<(), SharedMemoryError> {
        Err(SharedMemoryError::UnsupportedByPlatform)
    }

    /// Releases a shared-memory object entirely once no mapping of it remains in use anywhere.
    /// Idempotent-adjacent: real platforms (`CreateFileMappingW` `HANDLE`s, `memfd_create` fds)
    /// use OS-level refcounting, so this simply drops this holder's reference; the underlying
    /// memory is only actually freed once every holder (every process/fork that ever mapped this
    /// handle) has done so.
    ///
    /// The default implementation returns [`SharedMemoryError::UnsupportedByPlatform`]; see
    /// [`Self::create_shared_memory`]'s doc comment.
    #[expect(unused_variables, reason = "default body, non-underscored param names")]
    fn close_shared_memory(
        &self,
        handle: Self::SharedMemoryHandle,
    ) -> Result<(), SharedMemoryError> {
        Err(SharedMemoryError::UnsupportedByPlatform)
    }
}

/// Possible errors for [`PageManagementProvider::create_shared_memory`],
/// [`PageManagementProvider::map_shared_memory`],
/// [`PageManagementProvider::unmap_shared_memory`], and
/// [`PageManagementProvider::close_shared_memory`].
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum SharedMemoryError {
    #[error("this platform does not support real cross-process shared memory")]
    UnsupportedByPlatform,
    #[error("provided range is not page-aligned")]
    Unaligned,
    #[error("out of memory")]
    OutOfMemory,
    #[error("provided fixed address range is in use")]
    AddressInUse,
}

/// Behavior when allocating pages at a fixed address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixedAddressBehavior {
    /// The address is just a hint, and the platform may choose a different
    /// address if the hint is not available.
    Hint,
    /// Allocate the pages at the specified address, replacing any existing
    /// mappings.
    Replace,
    /// Allocate the pages at the specified address, failing if any part of the
    /// range is already in use.
    NoReplace,
}

/// Possible errors for [`PageManagementProvider::allocate_pages`]
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum AllocationError {
    #[error("provided range is not page-aligned")]
    Unaligned,
    #[error("provided address is below the minimum allowed address")]
    BelowMinAddress,
    #[error("provided address is above the maximum allowed address")]
    AboveMaxAddress,
    #[error("out of memory")]
    OutOfMemory,
    #[error("provided fixed address range is in use")]
    AddressInUse,
    #[error("provided fixed address range is in use by the platform")]
    AddressInUseByPlatform,
    #[error("provided fixed address range partially overlaps existing mappings")]
    AddressPartiallyInUse,
}

/// Possible errors for [`PageManagementProvider::deallocate_pages`]
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum DeallocationError {
    #[error("provided range is not page-aligned")]
    Unaligned,
    #[error("provided range contains unallocated pages")]
    AlreadyUnallocated,
}

/// Possible errors for [`PageManagementProvider::remap_pages`]
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum RemapError {
    #[error("at least one of the provided ranges was not page-aligned")]
    Unaligned,
    #[error("provided old range contains unallocated pages")]
    AlreadyUnallocated,
    #[error("provided ranges were overlapping")]
    Overlapping,
    #[error("provided new range is already allocated")]
    AlreadyAllocated,
    #[error("out of memory")]
    OutOfMemory,
}

/// Possible errors for [`PageManagementProvider::update_permissions`]
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum PermissionUpdateError {
    #[error("provided range is not page-aligned")]
    Unaligned,
    #[error("provided range contains unallocated pages")]
    Unallocated,
}

/// Possible errors for [`PageManagementProvider::try_allocate_cow_pages`]
///
/// ```text
///  ____________________
/// ( Maybe the grass is )
/// ( greener on the     )
/// ( other side?        )
///  --------------------
///         o   ^__^
///          o  (oo)\_______
///             (__)\       )\/\
///                 ||----w |
///                 ||     ||
/// ```
#[derive(Error, Debug)]
pub enum CowAllocationError {
    #[error("copy-on-write page allocation is not supported for this particular platform")]
    UnsupportedByPlatform,
    #[error("source region is not copy-on-writable")]
    UnsupportedSourceRegion,
    #[error("unaligned request")]
    Unaligned,
    #[error("internal failure in creating CoW pages")]
    InternalFailure,
}
