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

/// Entry addresses of every out-of-line function inside `buddy_system_allocator` that mutates
/// `SafeZoneAllocator`'s OWN buddy heap state (`Heap::<ORDER>::alloc`/`dealloc`, and the
/// `LockedHeapWithRescue<ORDER>` `GlobalAlloc` wrappers that call them directly) -- the same
/// "is `rip` currently inside the global allocator" ground truth [`slab_allocator_code_addrs`]
/// established for `slabmalloc::ZoneAllocator`, extended to the OTHER allocator
/// `SafeZoneAllocator` embeds.
///
/// # Why this exists -- found live, 2026-09-22, chasing the second Xvfb SIGSEGV
///
/// [`slab_allocator_code_addrs`]'s own doc comment already proved
/// `rip_in_global_allocator`'s old single-window assumption ("slabmalloc's `ZoneAllocator`
/// methods are inlined into `SafeZoneAllocator::alloc`/`dealloc`") FALSE -- they are genuinely
/// out-of-line, ~4.3 MB away in a real release build. The exact same assumption was made, and
/// never re-checked, for `buddy_system_allocator`'s OWN `Heap::alloc`/`Heap::dealloc` (called
/// directly from `LockedHeapWithRescue::alloc`/`dealloc`, in turn called directly from
/// `SafeZoneAllocator::alloc`/`dealloc` for the `BASE_PAGE_SIZE`/`LARGE_PAGE_SIZE`/large-object
/// cases) -- `Heap::dealloc` is not trivial (a buddy-merge loop walking `free_list`), the exact
/// shape of function LTO-off Rust reliably leaves out-of-line across a crate boundary (this
/// project builds with LTO off, see `AGENTS.md`'s own standing note on release-binary `cdb`
/// reads). Live-caught: a debug-build `de_only.sh` boot under `LITEBOX_PROCESS_FORK=1` hit a
/// reproducible `buddy_system_allocator-0.11.0/src/lib.rs:165` panic ("index out of bounds: the
/// len is 34 but the index is 53") on multiple unrelated host threads across one boot -- `34` is
/// this exact `ORDER` (`SafeZoneAllocator<'static, 34, WindowsUserland>`), and `53` is not a
/// class any real `Layout` reaching `Heap::alloc`/`dealloc` through `SafeZoneAllocator`'s own
/// size dispatch can produce, i.e. this is `Heap::free_list` bookkeeping corrupted by an earlier
/// thread that was interrupted mid-mutation -- the exact failure mode `slab_allocator_code_addrs`'s
/// own doc comment already describes and fixed for `ZoneAllocator`, just via `Heap`'s own
/// internal `spin::Mutex` (distinct from `SafeZoneAllocator::slab_allocator`'s `SpinMutex`), a
/// SEPARATE lock with the identical "no suspend-safety window" gap. Same crash address family as
/// AGENTS.md's tracked Xvfb SIGSEGV (a wild/stale pointer read) is consistent with this: any
/// corrupted allocator state can hand out a garbage pointer, or corrupt an unrelated live
/// allocation's bytes, to ordinary litebox host code staging guest-visible content.
#[doc(hidden)]
pub fn buddy_allocator_code_addrs<const ORDER: usize>() -> [usize; 4] {
    [
        buddy_system_allocator::Heap::<ORDER>::alloc as *const () as usize,
        buddy_system_allocator::Heap::<ORDER>::dealloc as *const () as usize,
        <buddy_system_allocator::LockedHeapWithRescue<ORDER> as GlobalAlloc>::alloc as *const ()
            as usize,
        <buddy_system_allocator::LockedHeapWithRescue<ORDER> as GlobalAlloc>::dealloc as *const ()
            as usize,
    ]
}

/// # Every internal failure panic below is deliberately allocation-free (60th pass, 2026-09-23)
///
/// `alloc`'s `OutOfMemory`/`InvalidLayout` branches and `dealloc`'s failure branch used to call
/// `.expect(msg)`/`panic!("{layout:?}")` while `slab_allocator.lock()`'s guard (`zone_allocator`)
/// was still held. `Result::expect` unconditionally formats `"{msg}: {error:?}"` -- and
/// interpolating any value at all (`{layout:?}` included) forces the same `format!` path -- which
/// allocates through this very type's own `#[global_allocator]` registration (`SLAB_ALLOC`,
/// `litebox_platform_windows_userland/src/lib.rs`). That reentrant allocation cannot be a
/// same-thread self-deadlock on `slab_allocator` itself (`spin::mutex::SpinMutex`'s guard still
/// runs its `Drop` during the unwind that formatting the panic message kicks off, releasing this
/// lock correctly) -- but a wholly UNRELATED spinlock that happened to be held by whatever code
/// path triggered that reentrant allocation has no such protection unless it was written with one.
/// Live-caught exactly this way: `litebox_platform_windows_userland::WaiterQueue::with_lock`
/// (`RawMutex`'s own internal waiter-registration spinlock) had no panic guard, and one of its
/// callers (`wake_many`'s `drain_locked`) allocates a `Vec` -- so a panic reaching this allocator
/// from inside that specific call wedged `WaiterQueue::lock` at `true` forever, live-confirmed via
/// `cdb -pv` (three snapshots, 40+ seconds apart, byte-identical `Child-SP`, `ssh-agent`'s exit
/// path stuck forever contending the wedged `RawMutex`). `WaiterQueue::with_lock` itself is now
/// fixed (a `litebox::utils::defer`-based release guard), but every panic site here is ALSO made
/// allocation-free as defense in depth: a plain `&'static str`-only `panic!` (no `{}`
/// placeholders) never reaches `format!`, so it can never recurse into this allocator no matter
/// which lock -- guarded or not, in this codebase or a future one -- happens to be held when it
/// fires.
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
                                // Deliberately a plain `match` + a `&'static str`-only `panic!`,
                                // never `.expect(msg)` -- see this impl block's own doc comment
                                // (60th pass) for why: `Result::expect` unconditionally formats
                                // `"{msg}: {error:?}"`, which allocates through this very
                                // `#[global_allocator]` (`SLAB_ALLOC`) while `zone_allocator`
                                // (this `slab_allocator.lock()` call's guard) is still held --
                                // live-caught wedging a DIFFERENT, unrelated spinlock
                                // (`WaiterQueue::with_lock`, `litebox_platform_windows_userland`)
                                // forever when a `Vec::push` inside it happened to be the
                                // allocation that panicked here. A plain string literal (no `{}`
                                // placeholders) never reaches `format!`, so it can never recurse
                                // into this allocator no matter which lock is held when it fires.
                                if unsafe { zone_allocator.refill(layout, page) }.is_err() {
                                    panic!("SafeZoneAllocator: refill failed");
                                }
                                let Ok(ptr) = zone_allocator.allocate(layout) else {
                                    panic!("SafeZoneAllocator: allocate failed right after refill");
                                };
                                ptr.as_ptr()
                            })
                        } else {
                            self.alloc_large_page()
                                .map_or(core::ptr::null_mut(), |large_page| {
                                    // See the sibling branch above for why this is a `match` +
                                    // static-only `panic!`, not `.expect(msg)`.
                                    if unsafe { zone_allocator.refill_large(layout, large_page) }
                                        .is_err()
                                    {
                                        panic!("SafeZoneAllocator: refill_large failed");
                                    }
                                    let Ok(ptr) = zone_allocator.allocate(layout) else {
                                        panic!(
                                            "SafeZoneAllocator: allocate failed right after refill_large"
                                        );
                                    };
                                    ptr.as_ptr()
                                })
                        }
                    }
                    Err(AllocationError::InvalidLayout) => {
                        // Static-only, deliberately not `panic!("Invalid layout: {layout:?}")`
                        // -- see this impl block's own doc comment: formatting `layout` would
                        // allocate while `zone_allocator` (this `slab_allocator.lock()` call's
                        // guard) is still held.
                        panic!("SafeZoneAllocator: invalid layout");
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
                    // Static-only `panic!`, not `.expect("Failed to deallocate")` -- see this
                    // impl block's `alloc`'s own doc comment: `.expect(msg)` always formats
                    // `"{msg}: {error:?}"`, which would allocate through this very
                    // `#[global_allocator]` while `slab_allocator.lock()`'s guard (the temporary
                    // this whole chained call holds) is still alive.
                    if self.slab_allocator.lock().deallocate(ptr, layout).is_err() {
                        panic!("SafeZoneAllocator: failed to deallocate");
                    }
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
