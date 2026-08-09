// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Memory management related functionality

pub mod allocator;
pub mod exception_table;
pub mod linux;

#[cfg(test)]
mod tests;

use core::ops::Range;

use alloc::vec::Vec;
use linux::{
    CreatePagesFlags, MappingError, PageFaultError, PageRange, VmArea, VmFlags, Vmem,
    VmemDuplicateError, VmemPageFaultHandler, VmemProtectError, VmemUnmapError,
};

use crate::{
    LiteBox,
    mm::linux::{NonZeroAddress, NonZeroPageSize, VmemResetError},
    platform::{
        PageManagementProvider, RawConstPointer,
        page_mgmt::{MemoryRegionPermissions, RemapError},
    },
    sync::{RawSyncPrimitivesProvider, RwLock},
};

/// A page manager to support `mmap`, `munmap`, and etc.
pub struct PageManager<Platform, const ALIGN: usize>
where
    Platform: RawSyncPrimitivesProvider + PageManagementProvider<ALIGN>,
{
    vmem: RwLock<Platform, Vmem<Platform, ALIGN>>,
}

/// The address relocations produced by [`PageManager::duplicate`]: maps each duplicated
/// source-address-space range to the (possibly different) base address it landed at in the
/// destination.
///
/// Used to translate any address captured from the source's address space -- most importantly
/// CPU register state that will be resumed in the new process -- into the corresponding
/// destination address.
pub struct AddressRelocations {
    ranges: Vec<(Range<usize>, usize)>,
    /// The source (parent) process's program break at the moment of duplication, i.e. the current
    /// upper bound of its heap (`brk`-allocated) region -- `0` if `set_initial_brk` was never
    /// called (no heap exists yet). By construction (`PageManager::brk`'s `create_pages` call),
    /// the heap VMA's tracked range always ends exactly here, which is what lets
    /// [`Self::heap_range`] identify it precisely rather than by heuristic.
    heap_top: usize,
}

impl AddressRelocations {
    /// Translate `addr` (assumed to be a valid address in the source address space at the time
    /// of duplication) into the corresponding address in the destination address space.
    ///
    /// Returns `None` if `addr` does not fall within any duplicated range (e.g. it is 0, or it
    /// points into a `VM_SHARED` mapping that duplication itself would have already rejected, or
    /// it is simply not a pointer at all and happens to not fall in any tracked range).
    #[must_use]
    pub fn translate(&self, addr: usize) -> Option<usize> {
        self.ranges
            .iter()
            .find(|(source_range, _)| source_range.contains(&addr))
            .map(|(source_range, dest_base)| dest_base + (addr - source_range.start))
    }

    /// Returns whether `addr` falls within one of the SOURCE (pre-duplication, i.e. parent)
    /// ranges.
    ///
    /// After `fork()`, the parent's original mappings are never unmapped, so any such address is
    /// still live, mapped host memory in the child's process too -- which is exactly why a stale,
    /// untranslated pointer copied verbatim into the child does not fault the way it would on
    /// real hardware. This predicate is the basis for detecting such stale pointers.
    #[must_use]
    pub fn is_in_source(&self, addr: usize) -> bool {
        self.ranges
            .iter()
            .any(|(source_range, _)| source_range.contains(&addr))
    }

    /// Returns whether `addr` falls within one of the DESTINATION (post-duplication, i.e. child)
    /// ranges.
    #[must_use]
    pub fn is_in_destination(&self, addr: usize) -> bool {
        self.ranges.iter().any(|(source_range, dest_base)| {
            (*dest_base..dest_base + source_range.len()).contains(&addr)
        })
    }

    /// Returns the `(source range, destination base)` pairs, for consumers (such as a platform's
    /// post-`fork()` execution verifier) that need to hold onto a snapshot of the mapping
    /// independently of this object.
    #[must_use]
    pub fn ranges(&self) -> &[(Range<usize>, usize)] {
        &self.ranges
    }

    /// Returns the destination-space range of the source process's heap (`brk`-allocated region)
    /// at the moment of duplication, if it has one (`set_initial_brk` was called and at least one
    /// successful `brk()` growth has happened, so a heap VMA actually exists and was duplicated).
    ///
    /// Identified precisely, not heuristically: `PageManager::brk`'s `create_pages` call always
    /// creates the heap VMA to end exactly at the current program break, so the (unique) tracked
    /// source range whose end equals [`Self::heap_top`] -- via `self.heap_top`, captured at
    /// duplication time -- *is* the heap, by construction of how that range came to exist. Used by
    /// [`crate`]'s consumers that need to exclude the heap from a broad memory scan (e.g. a
    /// fork-time stale-pointer fixup pass): a `call`-instruction return address, a spilled
    /// register, or a TCB field can never legitimately live in `brk`-allocated memory, so any
    /// pattern-matched "looks like a pointer" value found there is guaranteed to be ordinary
    /// program data (e.g. a shell's stack-string/argv-construction buffer) instead.
    #[must_use]
    pub fn heap_range(&self) -> Option<(Range<usize>, usize)> {
        if self.heap_top == 0 {
            return None;
        }
        self.ranges
            .iter()
            .find(|(source_range, _)| source_range.end == self.heap_top)
            .cloned()
    }
}

#[cfg(test)]
mod address_relocations_tests {
    use super::AddressRelocations;

    /// Regression coverage for the builtin-then-`execve` argv corruption this investigation
    /// root-caused: `fixup_stale_stack_pointers` (in `litebox_shim_linux`) must be able to
    /// identify and skip the heap range so its broad "does this 8-byte slot look like a
    /// translatable pointer" scan never touches live guest heap data (e.g. a shell's
    /// `stalloc`-style stack-string arena, confirmed live to live there in one investigated
    /// case) -- `heap_range` is the primitive that makes that possible, so its own
    /// identify-by-construction logic (matching on the tracked range whose end equals the
    /// captured `heap_top`) needs to be correct independent of the full `PageManager::duplicate`
    /// machinery (which cannot be exercised in a plain unit test without a real platform).
    #[test]
    fn heap_range_identifies_the_range_ending_at_heap_top() {
        let relocations = AddressRelocations {
            ranges: alloc::vec![
                // A stack-like range, much larger than the heap, whose end does NOT match
                // heap_top -- must never be mistaken for the heap.
                (0x7000_0000..0x7080_0000, 0x9000_0000),
                // The heap: ends exactly at heap_top, by construction of how PageManager::brk
                // creates it.
                (0x1000_0000..0x1010_0000, 0x2000_0000),
                // A small TCB-like range, also not ending at heap_top.
                (0x8000_0000..0x8000_2000, 0xa000_0000),
            ],
            heap_top: 0x1010_0000,
        };

        assert_eq!(
            relocations.heap_range(),
            Some((0x1000_0000..0x1010_0000, 0x2000_0000)),
            "must identify the range whose end matches heap_top, not any other range"
        );
    }

    /// `heap_top == 0` means `set_initial_brk` was never called for this process (no heap VMA
    /// exists at all yet) -- `heap_range` must report "no heap" rather than spuriously matching
    /// some unrelated range that happens to end at address 0 (which cannot happen for a real
    /// range, but the explicit early-return must still be exercised, not relied upon by
    /// coincidence).
    #[test]
    fn heap_range_is_none_when_no_heap_exists_yet() {
        let relocations = AddressRelocations {
            ranges: alloc::vec![(0x7000_0000..0x7080_0000, 0x9000_0000)],
            heap_top: 0,
        };

        assert_eq!(relocations.heap_range(), None);
    }

    /// If no tracked range's end happens to match `heap_top` (should not occur in practice, since
    /// `PageManager::brk` always creates the heap range to end there -- but `heap_range` must
    /// degrade gracefully rather than panicking or matching the wrong range if it ever does, e.g.
    /// a future refactor changing how the heap range is created).
    #[test]
    fn heap_range_is_none_when_no_range_matches_heap_top() {
        let relocations = AddressRelocations {
            ranges: alloc::vec![(0x7000_0000..0x7080_0000, 0x9000_0000)],
            heap_top: 0x1234_5678,
        };

        assert_eq!(relocations.heap_range(), None);
    }
}

impl<Platform, const ALIGN: usize> PageManager<Platform, ALIGN>
where
    Platform: RawSyncPrimitivesProvider + PageManagementProvider<ALIGN>,
{
    /// Create a new `PageManager` instance.
    pub fn new(litebox: &LiteBox<Platform>) -> Self {
        let vmem = RwLock::new(linux::Vmem::new(litebox.x.platform));
        Self { vmem }
    }

    /// Create a new `PageManager` for the same `Platform`, whose guest-tracked address space is
    /// an eager, independent copy of `self`'s current guest-tracked address space.
    ///
    /// This is the `fork()` primitive: intended to be called once, at the point a child process
    /// is created, with the parent's `PageManager`. Writes made through the returned
    /// `PageManager` (or through `self`) after this call do not affect the other.
    ///
    /// Also returns a [`AddressRelocations`], since the destination generally CANNOT be given
    /// the same addresses as the source (the platform may already have the source's addresses
    /// committed for its own use, e.g. on Windows) -- the caller MUST use it to translate any
    /// address captured from the source's address space (most importantly, CPU register state
    /// like `rsp`/`rbp` that will be resumed in the new process) into the corresponding
    /// destination address before using it, or the child will resume with dangling pointers into
    /// memory it does not own.
    ///
    /// # Errors
    ///
    /// Returns [`VmemDuplicateError`] if any tracked mapping cannot be duplicated -- in
    /// particular, a `VM_SHARED` mapping currently always fails this way.
    ///
    /// # Safety
    ///
    /// The caller must ensure no other code is concurrently mutating memory tracked by `self`
    /// for the duration of this call (e.g. other threads of the same process must be stopped).
    pub unsafe fn duplicate(
        &self,
        litebox: &LiteBox<Platform>,
    ) -> Result<(Self, AddressRelocations), VmemDuplicateError> {
        let source_vmem = self.vmem.read();
        let source_ranges: Vec<Range<usize>> = source_vmem.iter().map(|(r, _)| r.clone()).collect();
        let heap_top = source_vmem.brk;
        let mut dest_vmem =
            linux::Vmem::new_excluding(litebox.x.platform, source_ranges.into_iter());
        let relocations = unsafe { source_vmem.duplicate(&mut dest_vmem) }?;
        Ok((
            Self {
                vmem: RwLock::new(dest_vmem),
            },
            AddressRelocations {
                ranges: relocations,
                heap_top,
            },
        ))
    }

    /// Create a mapping with the given flags.
    ///
    /// `suggested_new_address` is the hint address for where to create the pages if it is not `None`.
    /// Otherwise, let the kernel choose an available memory region.
    ///
    /// `length` is the size of the pages to be created.
    ///
    /// Set `flags` to control options such as fixed address, stack, and populate pages.
    ///
    /// `op` is a callback for caller to initialize the created pages.
    ///
    /// `before_perms` and `after_perms` are the permissions to set before and after the call to `op`.
    ///
    /// # Safety
    ///
    /// Note that if the suggested address is given and [`CreatePagesFlags::FIXED_ADDR`] is set,
    /// the kernel uses it directly without checking if it is available, causing overlapping
    /// mappings to be unmapped. Caller must ensure any overlapping mappings are not used by any other.
    ///
    /// Also, caller must ensure flags are set correctly.
    unsafe fn create_pages<F>(
        &self,
        suggested_address: Option<NonZeroAddress<ALIGN>>,
        length: NonZeroPageSize<ALIGN>,
        flags: CreatePagesFlags,
        before_perms: MemoryRegionPermissions,
        after_perms: MemoryRegionPermissions,
        op: F,
    ) -> Result<Platform::RawMutPointer<u8>, MappingError>
    where
        F: FnOnce(Platform::RawMutPointer<u8>) -> Result<usize, MappingError>,
    {
        let addr = {
            let mut vmem = self.vmem.write();
            unsafe { vmem.create_pages(suggested_address, length, flags, before_perms) }?
        };
        // call the user function with the pages
        // Note `op` may trigger page fault handler which requires write lock to `vmem`.
        if let Err(e) = op(addr) {
            // remove the mapping if the user function fails
            let mut vmem = self.vmem.write();
            unsafe {
                vmem.remove_mapping(
                    PageRange::new(addr.as_usize(), addr.as_usize() + length.as_usize()).unwrap(),
                )
            }
            .unwrap();
            return Err(e);
        }
        if before_perms != after_perms {
            let range =
                PageRange::new(addr.as_usize(), addr.as_usize() + length.as_usize()).unwrap();
            // `protect` should succeed, as we just created the mapping.
            let mut vmem = self.vmem.write();
            unsafe { vmem.protect_mapping(range, after_perms) }.expect("failed to protect mapping");
        }
        Ok(addr)
    }

    /// Create readable and executable pages.
    ///
    /// `suggested_address` is the hint address for where to create the pages if it is not `None`.
    /// Otherwise, let the kernel choose an available memory region.
    ///
    /// `length` is the size of the pages to be created.
    ///
    /// Set `flags` to control options such as fixed address, stack, and populate pages.
    ///
    /// `op` is a callback for caller to initialize the created pages.
    ///
    /// # Safety
    ///
    /// If the suggested start address is given (i.e., not zero) and `fixed_addr` is set to `true`,
    /// the kernel uses it directly without checking if it is available, causing overlapping
    /// mappings to be unmapped. Caller must ensure any overlapping mappings are not used by any other.
    pub unsafe fn create_executable_pages<F>(
        &self,
        suggested_address: Option<NonZeroAddress<ALIGN>>,
        length: NonZeroPageSize<ALIGN>,
        flags: CreatePagesFlags,
        op: F,
    ) -> Result<Platform::RawMutPointer<u8>, MappingError>
    where
        F: FnOnce(Platform::RawMutPointer<u8>) -> Result<usize, MappingError>,
    {
        unsafe {
            self.create_pages(
                suggested_address,
                length,
                flags,
                // create READ | WRITE pages (as `op` may need to write to them, e.g., fill in the code)
                MemoryRegionPermissions::READ | MemoryRegionPermissions::WRITE,
                // keep READ, turn off WRITE and turn on EXEC
                MemoryRegionPermissions::READ | MemoryRegionPermissions::EXEC,
                op,
            )
        }
    }

    /// Create readable and writable pages.
    ///
    /// `suggested_address` is the hint address for where to create the pages if it is not `None`.
    /// Otherwise, let the kernel choose an available memory region.
    ///
    /// `length` is the size of the pages to be created.
    ///
    /// Set `flags` to control options such as fixed address, stack, and populate pages.
    ///
    /// `op` is a callback for caller to initialize the created pages.
    ///
    /// # Safety
    ///
    /// If the suggested start address is given (i.e., not zero) and `fixed_addr` is set to `true`,
    /// the kernel uses it directly without checking if it is available, causing overlapping
    /// mappings to be unmapped. Caller must ensure any overlapping mappings are not used by any other.
    pub unsafe fn create_writable_pages<F>(
        &self,
        suggested_address: Option<NonZeroAddress<ALIGN>>,
        length: NonZeroPageSize<ALIGN>,
        flags: CreatePagesFlags,
        op: F,
    ) -> Result<Platform::RawMutPointer<u8>, MappingError>
    where
        F: FnOnce(Platform::RawMutPointer<u8>) -> Result<usize, MappingError>,
    {
        let perms = MemoryRegionPermissions::READ | MemoryRegionPermissions::WRITE;
        unsafe { self.create_pages(suggested_address, length, flags, perms, perms, op) }
    }

    /// Create read-only pages.
    ///
    /// `suggested_address` is the hint address for where to create the pages if it is not `None`.
    /// Otherwise, let the kernel choose an available memory region.
    ///
    /// `length` is the size of the pages to be created.
    ///
    /// Set `flags` to control options such as fixed address, stack, and populate pages.
    ///
    /// `op` is a callback for caller to initialize the created pages.
    ///
    /// # Safety
    ///
    /// If the suggested start address is given (i.e., not zero) and `fixed_addr` is set to `true`,
    /// the kernel uses it directly without checking if it is available, causing overlapping
    /// mappings to be unmapped. Caller must ensure any overlapping mappings are not used by any other.
    pub unsafe fn create_readable_pages<F>(
        &self,
        suggested_address: Option<NonZeroAddress<ALIGN>>,
        length: NonZeroPageSize<ALIGN>,
        flags: CreatePagesFlags,
        op: F,
    ) -> Result<Platform::RawMutPointer<u8>, MappingError>
    where
        F: FnOnce(Platform::RawMutPointer<u8>) -> Result<usize, MappingError>,
    {
        unsafe {
            self.create_pages(
                suggested_address,
                length,
                flags,
                // create READ | WRITE pages (as `op` may need to write to them, e.g., fill in the data)
                MemoryRegionPermissions::READ | MemoryRegionPermissions::WRITE,
                // keep READ, turn off WRITE
                MemoryRegionPermissions::READ,
                op,
            )
        }
    }

    /// Create inaccessible pages.
    ///
    /// `suggested_address` is the hint address for where to create the pages if it is not `None`.
    /// Otherwise, let the kernel choose an available memory region.
    ///
    /// `length` is the size of the pages to be created.
    ///
    /// Set `flags` to control options such as fixed address, stack, and populate pages.
    ///
    /// `op` is a callback for caller to initialize the created pages.
    ///
    /// # Safety
    ///
    /// If the suggested start address is given (i.e., not zero) and `fixed_addr` is set to `true`,
    /// the kernel uses it directly without checking if it is available, causing overlapping
    /// mappings to be unmapped. Caller must ensure any overlapping mappings are not used by any other.
    pub unsafe fn create_inaccessible_pages<F>(
        &self,
        suggested_address: Option<NonZeroAddress<ALIGN>>,
        length: NonZeroPageSize<ALIGN>,
        flags: CreatePagesFlags,
        op: F,
    ) -> Result<Platform::RawMutPointer<u8>, MappingError>
    where
        F: FnOnce(Platform::RawMutPointer<u8>) -> Result<usize, MappingError>,
    {
        unsafe {
            self.create_pages(
                suggested_address,
                length,
                flags,
                MemoryRegionPermissions::empty(),
                MemoryRegionPermissions::empty(),
                op,
            )
        }
    }

    /// Create stack pages.
    ///
    /// `suggested_address` is the hint address for where to create the pages if it is not `None`.
    /// Otherwise, let the kernel choose an available memory region.
    ///
    /// `length` is the size of the pages to be created.
    ///
    /// Set `flags` to control options such as fixed address, stack, and populate pages.
    ///
    /// # Safety
    ///
    /// If the suggested start address is given (i.e., not zero) and `fixed_addr` is set to `true`,
    /// the kernel uses it directly without checking if it is available, causing overlapping
    /// mappings to be unmapped. Caller must ensure any overlapping mappings are not used by any other.
    pub unsafe fn create_stack_pages(
        &self,
        suggested_address: Option<NonZeroAddress<ALIGN>>,
        length: NonZeroPageSize<ALIGN>,
        flags: CreatePagesFlags,
    ) -> Result<Platform::RawMutPointer<u8>, MappingError> {
        let perms = MemoryRegionPermissions::READ | MemoryRegionPermissions::WRITE;
        let flags = CreatePagesFlags::IS_STACK | flags;
        unsafe { self.create_pages(suggested_address, length, flags, perms, perms, |_| Ok(0)) }
    }

    /// Set the initial program break address.
    ///
    /// This function should be called once to set the initial program break,
    /// which is usually the end of the data segment.
    ///
    /// # Panics
    ///
    /// Panics if the initial program break is already set.
    pub fn set_initial_brk(&self, brk: usize) {
        let mut vmem = self.vmem.write();
        assert_eq!(vmem.brk, 0, "initial brk is already set");
        vmem.brk = brk;
    }

    /// Set the program break to the given address.
    ///
    /// Increasing the program break has the effect of allocating memory to the process;
    /// decreasing the break deallocates memory.
    /// Calling `brk` with 0 can be used to find the current location of the program break.
    ///
    /// Note the initial program break is set to zero and the first call to `brk` would set it
    /// to the given address, which is usually the end of the data segment.
    ///
    /// ## Returns
    ///
    /// If the operation is successful, it returns the new program break address.
    ///
    /// # Panics
    ///
    /// Panics if the initial program break is not set yet.
    ///
    /// # Safety
    ///
    /// If shrinking the program break, the caller must ensure that the released memory region is no longer used.
    pub unsafe fn brk(&self, brk: usize) -> Result<usize, MappingError> {
        let mut vmem = self.vmem.write();
        assert_ne!(vmem.brk, 0, "initial brk is not set yet");
        if brk == 0 {
            // Calling `brk` with 0 can be used to find the current location of the program break.
            return Ok(vmem.brk);
        }

        let old_brk = vmem.brk.next_multiple_of(linux::PAGE_SIZE);
        let new_brk = brk.next_multiple_of(linux::PAGE_SIZE);
        if vmem.brk >= brk {
            // Shrink the memory region
            let brk = match unsafe {
                vmem.remove_mapping(
                    PageRange::new(new_brk, old_brk).ok_or(MappingError::UnAligned)?,
                )
            } {
                Ok(()) => {
                    vmem.brk = brk;
                    brk
                }
                Err(_) => {
                    vmem.brk // No change, return the old brk
                }
            };
            return Ok(brk);
        }

        if vmem.overlapping(old_brk..new_brk).next().is_some() {
            return Err(MappingError::OutOfMemory);
        }
        if let Some(range) = PageRange::<ALIGN>::new(old_brk, new_brk) {
            let (suggested_address, length) = range.start_and_length();
            let perms = MemoryRegionPermissions::READ | MemoryRegionPermissions::WRITE;
            unsafe {
                vmem.create_pages(
                    Some(suggested_address),
                    length,
                    CreatePagesFlags::FIXED_ADDR | CreatePagesFlags::POPULATE_PAGES_IMMEDIATELY,
                    perms,
                )
            }?;
        }
        vmem.brk = brk;
        Ok(brk)
    }

    /// Release memory mappings that satisfy the given condition and reset the program break.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the released memory regions are no longer used.
    pub unsafe fn release_memory(
        &self,
        releasable: fn(Range<usize>, VmFlags) -> bool,
    ) -> Result<(), VmemUnmapError> {
        for (r, vma) in self.mappings() {
            if !releasable(r.clone(), vma) {
                continue;
            }
            let mut vmem = self.vmem.write();
            let Some(range) = PageRange::new(r.start, r.end) else {
                unreachable!()
            };
            unsafe { vmem.remove_mapping(range) }?;
        }

        // reset brk
        let mut vmem = self.vmem.write();
        vmem.brk = 0;

        Ok(())
    }

    /// Expands (or shrinks) an existing memory mapping
    ///
    /// `old_addr` is the old address of the virtual memory block that you want to expand (or shrink).
    ///
    /// `old_size` is the size of the old memory block.
    ///
    /// `new_size` is the new size of the memory block.
    ///
    /// `may_move` indicates whether the memory block can be moved to a new address if there is not sufficient
    /// space to expand the old memory block at its current location.
    ///
    /// ## Returns
    ///
    /// If the operation is successful, it returns the new address of the memory block.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the memory region is no longer used by any other.
    pub unsafe fn remap_pages(
        &self,
        old_addr: Platform::RawMutPointer<u8>,
        old_size: usize,
        new_size: usize,
        may_move: bool,
    ) -> Result<Platform::RawMutPointer<u8>, RemapError> {
        let mut vmem = self.vmem.write();
        let old_range = PageRange::new(old_addr.as_usize(), old_addr.as_usize() + old_size)
            .ok_or(RemapError::Unaligned)?;
        match unsafe {
            vmem.resize_mapping(
                old_range,
                linux::NonZeroPageSize::new(new_size).ok_or(RemapError::Unaligned)?,
            )
        } {
            Ok(()) => Ok(old_addr),
            Err(linux::VmemResizeError::RangeOccupied(_)) => {
                // trying to remap a subset of an existing mapping
                if !may_move {
                    return Err(RemapError::OutOfMemory);
                }
                match unsafe {
                    vmem.move_mappings(
                        old_range,
                        None,
                        NonZeroPageSize::new(new_size).ok_or(RemapError::Unaligned)?,
                    )
                } {
                    Ok(new_addr) => Ok(new_addr),
                    Err(linux::VmemMoveError::OutOfMemory) => Err(RemapError::OutOfMemory),
                    Err(linux::VmemMoveError::UnAligned) => Err(RemapError::Unaligned),
                    Err(linux::VmemMoveError::RemapError(err)) => Err(err),
                }
            }
            Err(linux::VmemResizeError::NotExist(_)) => Err(RemapError::AlreadyUnallocated),
            Err(linux::VmemResizeError::InvalidAddr { .. }) => Err(RemapError::AlreadyAllocated),
            Err(linux::VmemResizeError::OutOfMemory) => Err(RemapError::OutOfMemory),
        }
    }

    /// Remove pages from the mapping.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the memory region is no longer used by any other.
    pub unsafe fn remove_pages(
        &self,
        ptr: Platform::RawMutPointer<u8>,
        len: usize,
    ) -> Result<(), VmemUnmapError> {
        let mut vmem = self.vmem.write();
        let start = ptr.as_usize();
        let range = PageRange::new(start, start + len).ok_or(VmemUnmapError::UnAligned)?;
        unsafe { vmem.remove_mapping(range) }
    }

    /// Reset pages without removing its mapping.
    ///
    /// If `anonymous_only` is true and any part of the range is non‑anonymous (i.e., file‑backed),
    /// returns `Err(VmemResetError::FileBacked)`.
    ///
    /// After calling this function, the memory region remains mapped, but its contents are invalidated.
    /// Subsequent accesses to the region will result in repopulating the memory contents, either from
    /// the underlying mapped file (for file-backed mappings, which is supported) or as zero-filled pages
    /// (for anonymous mappings).
    ///
    /// # Safety
    ///
    /// The caller must ensure that the memory contents in the affected region are no longer accessed or
    /// relied upon. Any pointers or references to the previous contents become invalid.
    pub unsafe fn reset_pages(
        &self,
        ptr: Platform::RawMutPointer<u8>,
        len: usize,
        anonymous_only: bool,
    ) -> Result<(), VmemResetError> {
        let mut vmem = self.vmem.write();
        let start = ptr.as_usize();
        let range = PageRange::new(start, start + len).ok_or(VmemResetError::UnAligned)?;
        unsafe { vmem.reset_pages(range, anonymous_only) }
    }

    /// Internal common function used by `make_pages_*` to change page permissions.
    fn change_page_permissions(
        &self,
        ptr: Platform::RawMutPointer<u8>,
        len: usize,
        new_permissions: MemoryRegionPermissions,
    ) -> Result<(), VmemProtectError> {
        let mut vmem = self.vmem.write();
        let start = ptr.as_usize();
        let range = PageRange::new(start, start + len)
            .ok_or(VmemProtectError::InvalidRange(start..start + len))?;
        unsafe { vmem.protect_mapping(range, new_permissions) }
    }

    /// Make pages readable and writable.
    ///
    /// # Safety
    ///
    /// The caller must ensure there is no concurrent `execute` access to the memory region.
    pub unsafe fn make_pages_writable(
        &self,
        ptr: Platform::RawMutPointer<u8>,
        len: usize,
    ) -> Result<(), VmemProtectError> {
        self.change_page_permissions(
            ptr,
            len,
            MemoryRegionPermissions::READ | MemoryRegionPermissions::WRITE,
        )
    }

    /// Make pages readable and executable.
    ///
    /// # Safety
    ///
    /// The caller must ensure there is no concurrent `write` access to the memory region.
    pub unsafe fn make_pages_executable(
        &self,
        ptr: Platform::RawMutPointer<u8>,
        len: usize,
    ) -> Result<(), VmemProtectError> {
        self.change_page_permissions(
            ptr,
            len,
            MemoryRegionPermissions::READ | MemoryRegionPermissions::EXEC,
        )
    }

    /// Make pages readable only.
    ///
    /// # Safety
    ///
    /// The caller must ensure there is no concurrent `write/execute` access to the memory region.
    pub unsafe fn make_pages_readable(
        &self,
        ptr: Platform::RawMutPointer<u8>,
        len: usize,
    ) -> Result<(), VmemProtectError> {
        self.change_page_permissions(ptr, len, MemoryRegionPermissions::READ)
    }

    /// Make pages inaccessible.
    ///
    /// # Safety
    ///
    /// The caller must ensure there is no concurrent access to the memory region.
    pub unsafe fn make_pages_inaccessible(
        &self,
        ptr: Platform::RawMutPointer<u8>,
        len: usize,
    ) -> Result<(), VmemProtectError> {
        self.change_page_permissions(ptr, len, MemoryRegionPermissions::empty())
    }

    /// Make pages readable, writable and executable.
    ///
    /// # Safety
    ///
    /// This operation is inherently dangerous and should be used with extreme caution.
    /// Allowing pages to be both writable and executable can lead to severe security vulnerabilities,
    /// such as code injection attacks or exploitation of memory corruption bugs.
    ///
    /// The caller must ensure the following:
    /// 1. The memory region is only used for legitimate purposes, such as JIT compilation,
    ///    where writable and executable permissions are strictly necessary.
    /// 2. The memory region is properly sanitized and does not contain malicious or unintended code.
    ///
    /// It is highly recommended to minimize the use of this function and to prefer safer alternatives
    /// whenever possible. If this function must be used, ensure that the memory region is locked down
    /// and access is strictly controlled.
    pub unsafe fn make_pages_rwx(
        &self,
        ptr: Platform::RawMutPointer<u8>,
        len: usize,
    ) -> Result<(), VmemProtectError> {
        self.change_page_permissions(
            ptr,
            len,
            MemoryRegionPermissions::READ
                | MemoryRegionPermissions::WRITE
                | MemoryRegionPermissions::EXEC,
        )
    }

    /// Register an already-allocated memory region in the VMA tracker.
    ///
    /// This is used when memory has been allocated by some means other than the normal
    /// `create_*_pages` path (e.g., CoW mappings created directly by the platform), so that the
    /// page manager tracks the region for future `mprotect`, `munmap`, etc.
    ///
    /// If `replace` is `true`, any overlapping tracked mappings are evicted from the tracker
    /// (without calling the platform deallocator) before inserting. Otherwise, returns `None`
    /// without registering if the provided `range` overlaps with any existing mapping.
    ///
    /// # Safety
    ///
    /// The `range` must be an already-mapped region with the given `permissions`.
    #[must_use]
    pub unsafe fn register_existing_mapping(
        &self,
        range: PageRange<ALIGN>,
        permissions: MemoryRegionPermissions,
        is_file_backed: bool,
        replace: bool,
        shared: bool,
    ) -> Option<()> {
        let vma = VmArea::new(
            VmFlags::from(permissions) | VmFlags::may_flags_for_mapping(shared, is_file_backed),
            is_file_backed,
        );
        let mut vmem = self.vmem.write();
        if !replace && vmem.overlapping(range.into()).next().is_some() {
            return None;
        }
        vmem.register_existing_mapping_overwrite(range, vma);
        Some(())
    }

    /// Returns all mappings in a vector.
    pub fn mappings(&self) -> Vec<(Range<usize>, VmFlags)> {
        self.vmem
            .read()
            .iter()
            .map(|(r, vma)| (r.start..r.end, vma.flags()))
            .collect()
    }

    /// Get the memory permissions of a given address range.
    ///
    /// `ptr` specifies the start address of the memory range.
    /// `len` specifies the length of the memory range.
    /// This function returns `MemoryRegionPermissions` only if the range is valid.
    /// A memory range is invalid if it contains:
    /// - Unmapped pages
    /// - Memory pages with different permissions
    pub fn get_memory_permissions(
        &self,
        ptr: NonZeroAddress<ALIGN>,
        len: NonZeroPageSize<ALIGN>,
    ) -> Option<MemoryRegionPermissions> {
        let vmem = self.vmem.read();
        let start = ptr.as_usize();
        let end = start + len.as_usize();
        let page_range = PageRange::<ALIGN>::new(start, end)?;
        vmem.get_memory_permissions(page_range)
    }
}

/// If Backend also implements [`VmemPageFaultHandler`], it can handle page faults.
impl<Platform, const ALIGN: usize> PageManager<Platform, ALIGN>
where
    Platform: RawSyncPrimitivesProvider + PageManagementProvider<ALIGN>,
    Platform: VmemPageFaultHandler,
{
    /// Handle page fault at the given address.
    ///
    /// # Safety
    ///
    /// This should only be called from the kernel page fault handler.
    pub unsafe fn handle_page_fault(
        &self,
        fault_addr: usize,
        error_code: u64,
    ) -> Result<(), PageFaultError> {
        let fault_addr = fault_addr & !(ALIGN - 1);
        if !(Platform::TASK_ADDR_MIN..Platform::TASK_ADDR_MAX).contains(&fault_addr) {
            return Err(PageFaultError::AccessError("Invalid address"));
        }

        let mut vmem = self.vmem.write();
        // Find the range closest to the fault address
        let (start, vma) = {
            let (r, vma) = vmem
                .overlapping(fault_addr..Platform::TASK_ADDR_MAX)
                .next()
                .ok_or(PageFaultError::AccessError("no mapping"))?;
            (r.start, *vma)
        };
        if fault_addr < start {
            // address is out of range, test if it is next to a stack
            if !vma.flags().contains(VmFlags::VM_GROWSDOWN) {
                return Err(PageFaultError::AccessError("no mapping"));
            }

            if !vmem
                .overlapping(Platform::TASK_ADDR_MIN..fault_addr)
                .next_back()
                .is_none_or(|(prev_range, prev_vma)| {
                    // Enforce gap between stack and other preceding non-stack mappings.
                    // Either the previous mapping is also a stack mapping w/ some access flags
                    // or the previous mapping is far enough from the fault address
                    (prev_vma.flags().contains(VmFlags::VM_GROWSDOWN)
                        && !(prev_vma.flags() & VmFlags::VM_ACCESS_FLAGS).is_empty())
                        || fault_addr - prev_range.end >= Vmem::<Platform, ALIGN>::STACK_GUARD_GAP
                })
            {
                return Err(PageFaultError::AllocationFailed);
            }
            let Some(range) = PageRange::new(fault_addr, start) else {
                unreachable!()
            };
            if let Err(err) = unsafe {
                vmem.insert_mapping(
                    range,
                    vma,
                    false,
                    crate::platform::page_mgmt::FixedAddressBehavior::NoReplace,
                )
            } {
                unimplemented!("failed to grow stack: {:?}", err)
            }
        }

        if <Platform as VmemPageFaultHandler>::access_error(error_code, vma.flags()) {
            return Err(PageFaultError::AccessError("access error"));
        }

        unsafe {
            vmem.platform
                .handle_page_fault(fault_addr, vma.flags(), error_code)
        }
    }
}
