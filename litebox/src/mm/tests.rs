// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use core::ops::Range;

use alloc::vec;
use alloc::vec::Vec;

use crate::{
    mm::linux::{CreatePagesFlags, NonZeroAddress},
    platform::{
        PageManagementProvider, RawConstPointer,
        page_mgmt::MemoryRegionPermissions,
        trivial_providers::{TransparentConstPtr, TransparentMutPtr},
    },
};
use zerocopy::{FromBytes, IntoBytes};

use super::linux::{
    NonZeroPageSize, PAGE_SIZE, PageRange, VmArea, VmFlags, Vmem, VmemProtectError,
    VmemResizeError, is_heap_range, is_private_data_range,
};

/// A dummy implementation of [`VmemBackend`] that does nothing.
struct DummyVmemBackend;

impl crate::platform::RawPointerProvider for DummyVmemBackend {
    type RawConstPointer<T: FromBytes> = TransparentConstPtr<T>;
    type RawMutPointer<T: FromBytes + IntoBytes> = TransparentMutPtr<T>;
}

#[expect(unused_variables, reason = "dummy/mock backend")]
impl crate::platform::PageManagementProvider<PAGE_SIZE> for DummyVmemBackend {
    #[cfg(target_os = "linux")]
    const TASK_ADDR_MIN: usize = 0x1_0000; // default linux config
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    const TASK_ADDR_MAX: usize = 0x7FFF_FFFF_F000; // (1 << 47) - PAGE_SIZE;
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    const TASK_ADDR_MAX: usize = 0xFFFF_FFFF_F000; // 48-bit VA space

    type SharedMemoryHandle = ();

    fn allocate_pages(
        &self,
        suggested_range: Range<usize>,
        initial_permissions: crate::platform::page_mgmt::MemoryRegionPermissions,
        can_grow_down: bool,
        populate_pages_immediately: bool,
        fixed_address_behavior: crate::platform::page_mgmt::FixedAddressBehavior,
    ) -> Result<Self::RawMutPointer<u8>, crate::platform::page_mgmt::AllocationError> {
        Ok(TransparentMutPtr::from_usize(suggested_range.start))
    }

    unsafe fn deallocate_pages(
        &self,
        range: Range<usize>,
    ) -> Result<(), crate::platform::page_mgmt::DeallocationError> {
        Ok(())
    }

    unsafe fn remap_pages(
        &self,
        old_range: Range<usize>,
        new_range: Range<usize>,
        permissions: crate::platform::page_mgmt::MemoryRegionPermissions,
    ) -> Result<Self::RawMutPointer<u8>, crate::platform::page_mgmt::RemapError> {
        Ok(TransparentMutPtr::from_usize(new_range.start))
    }

    unsafe fn update_permissions(
        &self,
        range: Range<usize>,
        new_permissions: crate::platform::page_mgmt::MemoryRegionPermissions,
    ) -> Result<(), crate::platform::page_mgmt::PermissionUpdateError> {
        Ok(())
    }

    fn reserved_pages(&self) -> impl Iterator<Item = &Range<usize>> {
        core::iter::empty()
    }
}

fn collect_mappings(vmm: &Vmem<DummyVmemBackend, PAGE_SIZE>) -> Vec<Range<usize>> {
    vmm.iter().map(|v| v.0.start..v.0.end).collect()
}

#[test]
fn test_vmm_mapping() {
    let start_addr: usize = 0x1_0000;
    let range = PageRange::new(start_addr, start_addr + 12 * PAGE_SIZE).unwrap();
    let mut vmm = Vmem::new(&DummyVmemBackend);

    // []
    unsafe {
        vmm.insert_mapping(
            range,
            VmArea::new(
                VmFlags::VM_READ | VmFlags::VM_MAYREAD | VmFlags::VM_MAYWRITE,
                false,
            ),
            false,
            crate::platform::page_mgmt::FixedAddressBehavior::Replace,
        )
    }
    .unwrap();
    // [(0x1_0000, 0x1_c000)]
    assert_eq!(
        collect_mappings(&vmm),
        vec![start_addr..start_addr + 12 * PAGE_SIZE]
    );

    unsafe {
        vmm.remove_mapping(
            PageRange::new(start_addr + 2 * PAGE_SIZE, start_addr + 4 * PAGE_SIZE).unwrap(),
        )
    }
    .unwrap();
    // [(0x1_0000, 0x1_2000), (0x1_4000, 0x1_c000)]
    assert_eq!(
        collect_mappings(&vmm),
        vec![
            start_addr..start_addr + 2 * PAGE_SIZE,
            start_addr + 4 * PAGE_SIZE..start_addr + 12 * PAGE_SIZE
        ]
    );

    assert!(matches!(
        unsafe {
            vmm.resize_mapping(
                PageRange::new(start_addr + 2 * PAGE_SIZE, start_addr + 3 * PAGE_SIZE).unwrap(),
                NonZeroPageSize::new(PAGE_SIZE * 2).unwrap(),
            )
        },
        // Failed to resize, remain [(0x1_0000, 0x1_2000), (0x1_4000, 0x1_c000)]
        Err(VmemResizeError::NotExist(_))
    ));

    assert!(matches!(
        unsafe {
            vmm.resize_mapping(
                PageRange::new(start_addr, start_addr + 3 * PAGE_SIZE).unwrap(),
                NonZeroPageSize::new(PAGE_SIZE * 4).unwrap(),
            )
        },
        // Failed to resize, remain [(0x1_0000, 0x1_2000), (0x1_4000, 0x1_c000)]
        Err(VmemResizeError::InvalidAddr { .. })
    ));

    assert!(matches!(
        unsafe {
            vmm.protect_mapping(
                PageRange::new(start_addr + 2 * PAGE_SIZE, start_addr + 4 * PAGE_SIZE).unwrap(),
                MemoryRegionPermissions::READ | MemoryRegionPermissions::WRITE,
            )
        },
        // Failed to protect, remain [(0x1_0000, 0x1_2000), (0x1_4000, 0x1_c000)]
        Err(VmemProtectError::InvalidRange(_))
    ));

    assert!(
        unsafe {
            vmm.resize_mapping(
                PageRange::new(start_addr, start_addr + 2 * PAGE_SIZE).unwrap(),
                NonZeroPageSize::new(PAGE_SIZE * 4).unwrap(),
            )
        }
        .is_ok()
    );
    // Grow and merge, [(0x1_0000, 0x1_c000)]
    assert_eq!(
        collect_mappings(&vmm),
        vec![start_addr..start_addr + 12 * PAGE_SIZE]
    );

    assert!(matches!(
        unsafe {
            vmm.protect_mapping(
                PageRange::new(start_addr, start_addr + 4 * PAGE_SIZE).unwrap(),
                MemoryRegionPermissions::READ | MemoryRegionPermissions::EXEC,
            )
        },
        // Failed to protect, remain [(0x1_0000, 0x1_c000)]
        Err(VmemProtectError::NoAccess { .. })
    ));

    assert!(
        unsafe {
            vmm.protect_mapping(
                PageRange::new(start_addr + 2 * PAGE_SIZE, start_addr + 4 * PAGE_SIZE).unwrap(),
                MemoryRegionPermissions::READ | MemoryRegionPermissions::WRITE,
            )
        }
        .is_ok()
    );
    // Change permission, [(0x1_0000, 0x1_2000), (0x1_2000, 0x1_4000), (0x1_4000, 0x1_c000)]
    assert_eq!(
        collect_mappings(&vmm),
        vec![
            start_addr..start_addr + 2 * PAGE_SIZE,
            start_addr + 2 * PAGE_SIZE..start_addr + 4 * PAGE_SIZE,
            start_addr + 4 * PAGE_SIZE..start_addr + 12 * PAGE_SIZE
        ]
    );

    // try to remap [0x1_2000, 0x1_4000)
    let r = PageRange::new(start_addr + 2 * PAGE_SIZE, start_addr + 4 * PAGE_SIZE).unwrap();
    assert!(matches!(
        unsafe { vmm.resize_mapping(r, NonZeroPageSize::new(PAGE_SIZE * 4).unwrap()) },
        Err(VmemResizeError::RangeOccupied(_))
    ));
    assert!(
        unsafe {
            vmm.move_mappings(
                r,
                Some(NonZeroAddress::new(start_addr + 12 * PAGE_SIZE).unwrap()),
                NonZeroPageSize::new(PAGE_SIZE * 4).unwrap(),
            )
        }
        .is_ok_and(|v| v.as_usize() == start_addr + 12 * PAGE_SIZE)
    );
    assert_eq!(
        collect_mappings(&vmm),
        vec![
            start_addr..start_addr + 2 * PAGE_SIZE,
            start_addr + 4 * PAGE_SIZE..start_addr + 12 * PAGE_SIZE,
            start_addr + 12 * PAGE_SIZE..start_addr + 16 * PAGE_SIZE
        ]
    );

    // create new mapping with no suggested address
    assert_eq!(
        unsafe {
            vmm.create_mapping(
                None,
                NonZeroPageSize::new(PAGE_SIZE).unwrap(),
                VmArea::new(VmFlags::VM_READ | VmFlags::VM_MAYREAD, false),
                CreatePagesFlags::empty(),
            )
        }
        .unwrap()
        .as_usize(),
        DummyVmemBackend::TASK_ADDR_MAX - PAGE_SIZE,
    );
    assert_eq!(
        collect_mappings(&vmm),
        vec![
            start_addr..start_addr + 2 * PAGE_SIZE,
            start_addr + 4 * PAGE_SIZE..start_addr + 12 * PAGE_SIZE,
            start_addr + 12 * PAGE_SIZE..start_addr + 16 * PAGE_SIZE,
            DummyVmemBackend::TASK_ADDR_MAX - PAGE_SIZE..DummyVmemBackend::TASK_ADDR_MAX,
        ]
    );

    // create new mapping with fixed address that overlaps with other mapping
    assert_eq!(
        unsafe {
            vmm.create_mapping(
                Some(NonZeroAddress::new(start_addr + PAGE_SIZE).unwrap()),
                NonZeroPageSize::new(PAGE_SIZE).unwrap(),
                VmArea::new(VmFlags::VM_READ | VmFlags::VM_MAYREAD, false),
                CreatePagesFlags::FIXED_ADDR,
            )
        }
        .unwrap()
        .as_usize(),
        start_addr + PAGE_SIZE
    );
    assert_eq!(
        collect_mappings(&vmm),
        vec![
            start_addr..start_addr + PAGE_SIZE,
            start_addr + PAGE_SIZE..start_addr + 2 * PAGE_SIZE,
            start_addr + 4 * PAGE_SIZE..start_addr + 12 * PAGE_SIZE,
            start_addr + 12 * PAGE_SIZE..start_addr + 16 * PAGE_SIZE,
            DummyVmemBackend::TASK_ADDR_MAX - PAGE_SIZE..DummyVmemBackend::TASK_ADDR_MAX,
        ]
    );

    // shrink mapping
    assert!(
        unsafe {
            vmm.resize_mapping(
                PageRange::new(start_addr + 4 * PAGE_SIZE, start_addr + 8 * PAGE_SIZE).unwrap(),
                NonZeroPageSize::new(2 * PAGE_SIZE).unwrap(),
            )
        }
        .is_ok()
    );
    assert_eq!(
        collect_mappings(&vmm),
        vec![
            start_addr..start_addr + PAGE_SIZE,
            start_addr + PAGE_SIZE..start_addr + 2 * PAGE_SIZE,
            start_addr + 4 * PAGE_SIZE..start_addr + 6 * PAGE_SIZE,
            start_addr + 8 * PAGE_SIZE..start_addr + 12 * PAGE_SIZE,
            start_addr + 12 * PAGE_SIZE..start_addr + 16 * PAGE_SIZE,
            DummyVmemBackend::TASK_ADDR_MAX - PAGE_SIZE..DummyVmemBackend::TASK_ADDR_MAX,
        ]
    );
}

/// `is_heap_range` must identify the range ending exactly at the captured `brk` -- the same
/// identification `AddressRelocations::heap_range` uses -- and nothing else.
#[test]
fn is_heap_range_matches_only_the_range_ending_at_brk() {
    assert!(is_heap_range(&(0x1000..0x2000), 0x2000));
    assert!(!is_heap_range(&(0x1000..0x2000), 0x3000));
    // `heap_top == 0` means no heap VMA exists yet: never a match, even for a range that happens
    // to end at 0 (which cannot occur for a real VMA anyway).
    assert!(!is_heap_range(&(0x1000..0x2000), 0));
}

/// Regression test for the argv-corruption bug: a private/writable/non-exec/non-stack range that
/// is ALSO the `brk` heap must be excluded from `is_private_data_range`, even though it would
/// otherwise satisfy every other criterion -- see that function's doc comment ("Why the heap is
/// excluded") for the live repro (`apk add nodejs` + `node --version`) that motivated this
/// exclusion: a fork-time fixup pass consuming `private_data_ranges()` corrupted a live
/// heap-allocated argv string's NUL terminator because the heap was, before this fix, swept
/// unconditionally alongside genuine ELF `.data`/`.bss` segments.
#[test]
fn heap_range_is_excluded_from_private_data_even_when_otherwise_qualifying() {
    // A `VmArea` shaped exactly like a private, writable, non-executable, non-stack region -- the
    // same shape as a qualifying ELF `.data`/`.bss` segment -- so the ONLY thing that can exclude
    // it is the heap check itself.
    let heap_vma =
        VmArea::<DummyVmemBackend, PAGE_SIZE>::new(VmFlags::VM_READ | VmFlags::VM_WRITE, false);
    let heap_like_range = 0x5000..0x6000;
    let brk = 0x6000; // matches heap_like_range.end

    assert!(
        !is_private_data_range(&heap_vma, &heap_like_range, brk),
        "a range ending at the captured brk must never be treated as private data, regardless \
         of its VmFlags shape"
    );

    // Sanity check: the identical VMA/range shape IS accepted as private data once it no longer
    // coincides with the heap (brk elsewhere) -- proves the heap check is what's doing the
    // excluding above, not some other unrelated criterion.
    assert!(is_private_data_range(&heap_vma, &heap_like_range, 0x9999));
}

/// A genuinely stack-shaped range (`VM_GROWSDOWN`) must still be excluded regardless of the heap
/// check, confirming the two exclusions are independent (not accidentally aliased).
#[test]
fn stack_range_is_still_excluded_independent_of_heap_check() {
    let stack_vma = VmArea::<DummyVmemBackend, PAGE_SIZE>::new(
        VmFlags::VM_READ | VmFlags::VM_WRITE | VmFlags::VM_GROWSDOWN,
        false,
    );
    let stack_range = 0x7000_0000..0x7080_0000;
    assert!(!is_private_data_range(&stack_vma, &stack_range, 0));
}
