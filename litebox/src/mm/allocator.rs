// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Allocator that uses buddy allocator for pages and slab allocator for small objects.

use core::{
    alloc::{GlobalAlloc, Layout},
    ptr::NonNull,
};

use buddy_system_allocator::LockedHeapWithRescue;
use slabmalloc::{AllocationError, Allocator, LargeObjectPage, ObjectPage, ZoneAllocator};
use spin::mutex::SpinMutex;

/// Memory provider trait for global allocator.
///
/// TODO: consider taking a `&mut self` to allow for more flexibility in the future.
pub trait MemoryProvider {
    /// For page allocation from host.
    ///
    /// Note this is only called when the allocator is out of memory.
    /// To add memory to the allocator at any time (e.g., initialize the allocator with
    /// pre-allocated fixed-size memory), use [`SafeZoneAllocator::fill_pages`].
    ///
    /// It can return more than requested size. On success, it returns the start address
    /// and the size of the allocated memory.
    fn alloc(layout: &Layout) -> Option<(usize, usize)>;

    /// Returns the memory back to host.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the `addr` is valid and was allocated by [`Self::alloc`].
    unsafe fn free(addr: usize);
}

/// Allocator that uses buddy allocator for pages and slab allocator for small objects.
///
/// `ORDER` is the maximum order of the buddy allocator, specifying the maximum size of the
/// allocation that can be done using the buddy allocator -- i.e., 1 << (ORDER - 1).
pub struct SafeZoneAllocator<'a, const ORDER: usize, M: MemoryProvider> {
    buddy_allocator: LockedHeapWithRescue<ORDER>,
    slab_allocator: SpinMutex<ZoneAllocator<'a>>,
    memory_provider: core::marker::PhantomData<M>,
}

impl<const ORDER: usize, M: MemoryProvider> Default for SafeZoneAllocator<'_, ORDER, M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const ORDER: usize, M: MemoryProvider> SafeZoneAllocator<'_, ORDER, M> {
    const PAGE_SIZE: usize = 4096;
    /// 4 KiB
    const BASE_PAGE_SIZE: usize = 4096;
    /// 2 MiB
    const LARGE_PAGE_SIZE: usize = 2 * 1024 * 1024;
    const BASE_PAGE_SIZE_ORDER: u32 = (Self::BASE_PAGE_SIZE / Self::PAGE_SIZE).trailing_zeros();
    const LARGE_PAGE_SIZE_ORDER: u32 = (Self::LARGE_PAGE_SIZE / Self::PAGE_SIZE).trailing_zeros();

    pub const fn new() -> Self {
        Self {
            buddy_allocator: LockedHeapWithRescue::new(|heap, layout| {
                let page_aligned_size = layout.size().next_power_of_two();
                if page_aligned_size.trailing_zeros() as usize >= ORDER {
                    unimplemented!("requested size {page_aligned_size:#} is too large");
                }
                let Ok(layout) = Layout::from_size_align(page_aligned_size, page_aligned_size)
                else {
                    unreachable!();
                };
                if let Some((start, size)) = M::alloc(&layout) {
                    // the returned size might be larger than requested (i.e., layout.size())
                    unsafe { heap.add_to_heap(start, start + size) };
                }
            }),
            slab_allocator: SpinMutex::new(ZoneAllocator::new()),
            memory_provider: core::marker::PhantomData,
        }
    }

    /// Adds a range of memory to allow it to be controlled by the buddy allocator.
    /// Morally, the buddy allocator takes ownership of this range of memory.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the memory range is valid and not used by any others.
    pub unsafe fn fill_pages(&self, addr: usize, size: usize) {
        unsafe { self.buddy_allocator.lock().add_to_heap(addr, addr + size) };
    }

    /// Allocates a new [`ObjectPage`] from the System.
    fn alloc_page(&self) -> Option<&'static mut ObjectPage<'static>> {
        self.allocate_pages(Self::BASE_PAGE_SIZE_ORDER).map(|r| {
            if (r as usize).is_multiple_of(core::mem::align_of::<ObjectPage<'static>>()) {
                unsafe { &mut *r.cast() }
            } else {
                unreachable!()
            }
        })
    }

    /// Allocates a new [`LargeObjectPage`] from the system.
    fn alloc_large_page(&self) -> Option<&'static mut LargeObjectPage<'static>> {
        self.allocate_pages(Self::LARGE_PAGE_SIZE_ORDER).map(|r| {
            if (r as usize).is_multiple_of(core::mem::align_of::<LargeObjectPage<'static>>()) {
                unsafe { &mut *r.cast() }
            } else {
                unreachable!()
            }
        })
    }

    /// Allocate (1 << `order`) virtually contiguous pages using buddy allocator.
    pub fn allocate_pages(&self, order: u32) -> Option<*mut u8> {
        let ptr = unsafe {
            self.buddy_allocator.alloc(
                Layout::from_size_align(
                    Self::BASE_PAGE_SIZE << order,
                    Self::BASE_PAGE_SIZE << order,
                )
                .ok()?,
            )
        };
        if ptr.is_null() { None } else { Some(ptr) }
    }

    /// De-allocates virtually contiguous pages returned from [`SafeZoneAllocator::allocate_pages`].
    ///
    /// # Safety
    ///
    /// The caller must ensure:
    ///
    /// * `ptr` is a block of memory currently allocated via this allocator and,
    ///
    /// * `order` is the same that was used to allocate that block of memory.
    ///
    /// # Panics
    ///
    /// Panics if `order` is greater than `ORDER`.
    pub unsafe fn free_pages(&self, ptr: *mut u8, order: u32) {
        assert!(order as usize <= ORDER);
        unsafe {
            self.buddy_allocator.dealloc(
                ptr,
                Layout::from_size_align(
                    Self::BASE_PAGE_SIZE << order,
                    Self::BASE_PAGE_SIZE << order,
                )
                .unwrap(),
            );
        };
    }
}

/// Entry addresses of every out-of-line function that mutates [`SafeZoneAllocator`]'s internal
/// slab/buddy state, for callers that must detect "is this thread's `rip` currently inside the
/// global allocator?"
///
/// This exists because the platform layer's own guard against suspending a thread mid-allocation
/// (`litebox_platform_windows_userland`'s `rip_in_global_allocator`/`ThreadHandle::interrupt`)
/// originally approximated that check as "is `rip` within a fixed 512 KiB window centered on
/// `<SafeZoneAllocator as GlobalAlloc>::alloc`'s entry", on the stated assumption that the
/// window was "wide enough to comfortably cover that function, `dealloc`, and their
/// monomorphized/inlined callees (`slabmalloc`'s `ZoneAllocator::allocate`/`deallocate`,
/// `refill`, the buddy allocator)".
///
/// **That assumption was confirmed live to be false.** In a real release build of
/// `litebox_runner_linux_on_windows_userland.exe`, `llvm-symbolizer` over the shipped binary
/// places `<SafeZoneAllocator as GlobalAlloc>::alloc` at RVA `0x523c00..0x524f40` and `dealloc`
/// at `0x524f80..0x525240` (so the 512 KiB window spans `0x4a3c00..0x5a3c00`), but the linker
/// placed every `slabmalloc::ZoneAllocator` method roughly 4.3 MB away, far outside it:
///
/// * `ZoneAllocator::deallocate` -- RVA `0x9bfc00..0x9c0380`
/// * `ZoneAllocator::refill_large` -- RVA `0x9c03c0..0x9c06c0`
/// * `ZoneAllocator::refill` -- RVA `0x9c0700..0x9c0a00`
/// * `ZoneAllocator::allocate` -- RVA `0x9c0a40..0x9c27c0`
///
/// They are genuinely out-of-line (not inlined into `alloc`/`dealloc` as the window's author
/// expected), so the guard was a no-op for the overwhelming majority of the time a thread
/// actually spends mutating slab state -- `ZoneAllocator::allocate` alone is ~7.6 KiB of code
/// and holds the `SpinMutex` across page-list walks, `first_fit` bitfield scans, and
/// partial/full/empty list migrations. A thread suspended anywhere in that range was never
/// detected, had its `Rip` redirected to the interrupt callback, and abandoned the mutation
/// partway, corrupting the shared allocator for every other thread.
///
/// Returning the real addresses here replaces that proximity guess with ground truth. Callers
/// still apply their own conservative per-function window (function *extents* are not knowable
/// without runtime debug symbols), but now anchored on every relevant function rather than on
/// one of them.
#[doc(hidden)]
pub fn slab_allocator_code_addrs() -> [usize; 4] {
    // `as` casts through a concrete monomorphization; the trait methods are the real out-of-line
    // symbols the linker emitted, which is exactly what a `rip` comparison needs.
    [
        <ZoneAllocator<'static> as Allocator<'static>>::allocate as *const () as usize,
        <ZoneAllocator<'static> as Allocator<'static>>::deallocate as *const () as usize,
        <ZoneAllocator<'static> as Allocator<'static>>::refill as *const () as usize,
        <ZoneAllocator<'static> as Allocator<'static>>::refill_large as *const () as usize,
    ]
}

unsafe impl<const ORDER: usize, M: MemoryProvider> GlobalAlloc
    for SafeZoneAllocator<'static, ORDER, M>
{
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        match layout.size() {
            Self::BASE_PAGE_SIZE => {
                // Best to use the underlying backend directly to allocate pages
                // to avoid fragmentation
                self.allocate_pages(Self::BASE_PAGE_SIZE_ORDER)
                    .expect("allocate page")
            }
            Self::LARGE_PAGE_SIZE => {
                // Best to use the underlying backend directly to allocate large pages
                // to avoid fragmentation
                self.allocate_pages(Self::LARGE_PAGE_SIZE_ORDER)
                    .expect("allocate large page")
            }
            0..=ZoneAllocator::MAX_ALLOC_SIZE => {
                let mut zone_allocator = self.slab_allocator.lock();
                match zone_allocator.allocate(layout) {
                    Ok(ptr) => ptr.as_ptr(),
                    Err(AllocationError::OutOfMemory) => {
                        if layout.size() <= ZoneAllocator::MAX_BASE_ALLOC_SIZE {
                            self.alloc_page().map_or(core::ptr::null_mut(), |page| {
                                unsafe {
                                    zone_allocator
                                        .refill(layout, page)
                                        .expect("Could not refill?");
                                }
                                zone_allocator
                                    .allocate(layout)
                                    .expect("Should succeed after refill")
                                    .as_ptr()
                            })
                        } else {
                            self.alloc_large_page()
                                .map_or(core::ptr::null_mut(), |large_page| {
                                    unsafe {
                                        zone_allocator
                                            .refill_large(layout, large_page)
                                            .expect("Could not refill?");
                                    }
                                    zone_allocator
                                        .allocate(layout)
                                        .expect("Should succeed after refill")
                                        .as_ptr()
                                })
                        }
                    }
                    Err(AllocationError::InvalidLayout) => {
                        panic!("Invalid layout: {layout:?}");
                    }
                }
            }
            _ => unsafe { self.buddy_allocator.alloc(layout) },
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        match layout.size() {
            Self::BASE_PAGE_SIZE | Self::LARGE_PAGE_SIZE => unsafe {
                self.buddy_allocator.dealloc(ptr, layout);
            },
            0..=ZoneAllocator::MAX_ALLOC_SIZE => {
                if let Some(ptr) = NonNull::new(ptr) {
                    self.slab_allocator
                        .lock()
                        .deallocate(ptr, layout)
                        .expect("Failed to deallocate");
                }

                // TODO: An proper reclamation strategy could be implemented here
                // to release empty pages back from the ZoneAllocator to the buddy allocator.
            }
            _ => unsafe {
                self.buddy_allocator.dealloc(ptr, layout);
            },
        }
    }
}
