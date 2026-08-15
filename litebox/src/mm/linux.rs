// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! This module implements a virtual memory manager `Vmem` that manages virtual address spaces
//! backed by a memory [backend](PageManagementProvider). It provides functionality to create, remove, resize,
//! move, and protect memory mappings within a process's virtual address space.

use core::ops::Range;

use alloc::vec::Vec;
use rangemap::RangeMap;
use thiserror::Error;

use crate::platform::PageManagementProvider;
use crate::platform::RawConstPointer;
use crate::platform::RawMutPointer;
use crate::platform::page_mgmt::AllocationError;
use crate::platform::page_mgmt::DeallocationError;
use crate::platform::page_mgmt::FixedAddressBehavior;
use crate::platform::page_mgmt::MemoryRegionPermissions;
use crate::platform::page_mgmt::SharedMemoryError;

/// Page size in bytes
pub const PAGE_SIZE: usize = 4096;

bitflags::bitflags! {
    /// Flags to describe the properties of a memory region.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct VmFlags: u32 {
        /// Readable.
        const VM_READ = 1 << 0;
        /// Writable.
        const VM_WRITE = 1 << 1;
        /// Executable.
        const VM_EXEC = 1 << 2;
        /// Shared between processes.
        const VM_SHARED = 1 << 3;

        /* limits for mprotect() etc */
        /// `mprotect` can turn on VM_READ
        const VM_MAYREAD = 1 << 4;
        /// `mprotect` can turn on VM_WRITE
        const VM_MAYWRITE = 1 << 5;
        /// `mprotect` can turn on VM_EXEC
        const VM_MAYEXEC = 1 << 6;
        /// `mprotect` can turn on VM_SHARED
        const VM_MAYSHARE = 1 << 7;

        /// The area can grow downward upon page fault.
        const VM_GROWSDOWN = 1 << 8;

        const VM_ACCESS_FLAGS = Self::VM_READ.bits()
            | Self::VM_WRITE.bits()
            | Self::VM_EXEC.bits();
        const VM_MAY_ACCESS_FLAGS = Self::VM_MAYREAD.bits()
            | Self::VM_MAYWRITE.bits()
            | Self::VM_MAYEXEC.bits();
    }
}

impl VmFlags {
    /// Compute the default `VM_MAY*` and `VM_SHARED` flags for a mapping.
    ///
    /// Write permission (`VM_MAYWRITE`) is restricted only for shared **file-backed**
    /// mappings, because writes cannot be propagated back to the underlying file.
    pub(super) fn may_flags_for_mapping(shared: bool, file_backed: bool) -> Self {
        let restrict_write = shared && file_backed;
        let may = if restrict_write {
            Self::VM_MAY_ACCESS_FLAGS & !Self::VM_MAYWRITE
        } else {
            Self::VM_MAY_ACCESS_FLAGS
        };
        let shared_flag = if shared {
            Self::VM_SHARED
        } else {
            Self::empty()
        };
        may | shared_flag
    }
}

impl From<MemoryRegionPermissions> for VmFlags {
    fn from(value: MemoryRegionPermissions) -> Self {
        let mut flags = VmFlags::empty();
        flags.set(
            VmFlags::VM_READ,
            value.contains(MemoryRegionPermissions::READ),
        );
        flags.set(
            VmFlags::VM_WRITE,
            value.contains(MemoryRegionPermissions::WRITE),
        );
        flags.set(
            VmFlags::VM_EXEC,
            value.contains(MemoryRegionPermissions::EXEC),
        );
        if value.contains(MemoryRegionPermissions::SHARED) {
            unimplemented!("SHARED permission is not supported yet");
        }
        flags
    }
}

impl From<VmFlags> for MemoryRegionPermissions {
    fn from(value: VmFlags) -> Self {
        let mut flags = MemoryRegionPermissions::empty();
        flags.set(
            MemoryRegionPermissions::READ,
            value.contains(VmFlags::VM_READ),
        );
        flags.set(
            MemoryRegionPermissions::WRITE,
            value.contains(VmFlags::VM_WRITE),
        );
        flags.set(
            MemoryRegionPermissions::EXEC,
            value.contains(VmFlags::VM_EXEC),
        );
        flags.set(
            MemoryRegionPermissions::SHARED,
            value.contains(VmFlags::VM_SHARED),
        );
        flags
    }
}

pub const DEFAULT_RESERVED_SPACE_SIZE: usize = 0x100_0000; // 16 MiB

bitflags::bitflags! {
    /// Options for page creation.
    pub struct CreatePagesFlags: u8 {
        /// Force the mapping to be created at the given address, resulting in any
        /// existing overlapping mappings being removed.
        const FIXED_ADDR     = 1 << 0;
        /// The mapping is used for stack.
        const IS_STACK       = 1 << 1;
        /// Populate the pages immediately.
        const POPULATE_PAGES_IMMEDIATELY = 1 << 2;
        /// Ensure there is more space (i.e., `DEFAULT_RESERVED_SPACE_SIZE`) after the
        /// mapping so that user can grow the mapping later.
        const ENSURE_SPACE_AFTER = 1 << 3;
        // This flag indicates that the mapping is backed by a file.
        const MAP_FILE = 1 << 4;
        /// When combined with [`Self::FIXED_ADDR`], fail with [`AllocationError::AddressInUse`]
        /// if any part of the range is already mapped, instead of replacing existing mappings.
        const NOREPLACE = 1 << 5;
        /// The mapping is shared.
        const SHARED = 1 << 6;
    }
}

/// A non-empty range of page-aligned addresses
#[derive(Clone, Copy)]
pub struct PageRange<const ALIGN: usize> {
    /// Start page of the range.
    pub start: usize,
    /// End page of the range.
    pub end: usize,
}

impl<const ALIGN: usize> From<PageRange<ALIGN>> for Range<usize> {
    fn from(range: PageRange<ALIGN>) -> Self {
        range.start..range.end
    }
}

impl<const ALIGN: usize> IntoIterator for PageRange<ALIGN> {
    type Item = usize;
    type IntoIter = core::iter::StepBy<Range<usize>>;

    fn into_iter(self) -> Self::IntoIter {
        (self.start..self.end).step_by(ALIGN)
    }
}

impl<const ALIGN: usize> PageRange<ALIGN> {
    /// Create a new [`PageRange`].
    ///
    /// Returns `None` if the range is not `ALIGN`-aligned or empty.
    pub fn new(start: usize, end: usize) -> Option<Self> {
        if !start.is_multiple_of(ALIGN) || !end.is_multiple_of(ALIGN) {
            return None;
        }
        if start >= end {
            return None;
        }
        Some(Self { start, end })
    }

    /// Get the size of this `ALIGN`-aligned range
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// Whether the range is empty or not
    ///
    /// Note this range is never empty.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Get the start address and length of this range as a tuple.
    #[allow(
        clippy::missing_panics_doc,
        reason = "This function should not fail as the range is guaranteed to be non-empty and aligned."
    )]
    pub fn start_and_length(&self) -> (NonZeroAddress<ALIGN>, NonZeroPageSize<ALIGN>) {
        (
            NonZeroAddress::new(self.start).unwrap(),
            NonZeroPageSize::new(self.len()).unwrap(),
        )
    }
}

/// A non-zero `ALIGN`-aligned size in bytes.
#[derive(Clone, Copy)]
pub struct NonZeroPageSize<const ALIGN: usize> {
    size: usize,
}

impl<const ALIGN: usize> NonZeroPageSize<ALIGN> {
    /// Create a new non-zero `ALIGN`-aligned size.
    ///
    /// Returns `None` if the size is zero or not `ALIGN`-aligned.
    pub fn new(size: usize) -> Option<Self> {
        if size == 0 || !size.is_multiple_of(ALIGN) {
            return None;
        }
        Some(Self { size })
    }

    /// Get the size
    #[inline]
    pub fn as_usize(self) -> usize {
        self.size
    }
}

impl<const ALIGN: usize> core::ops::Add<usize> for NonZeroPageSize<ALIGN> {
    type Output = Option<Self>;

    fn add(self, rhs: usize) -> Self::Output {
        NonZeroPageSize::new(self.size + rhs)
    }
}

/// A non-zero address that is `ALIGN`-aligned.
#[derive(Clone, Copy)]
pub struct NonZeroAddress<const ALIGN: usize>(usize);

impl<const ALIGN: usize> NonZeroAddress<ALIGN> {
    /// Create a new `NonZeroAddress`, if the address is non-zero and aligned.
    pub fn new(address: usize) -> Option<Self> {
        if address == 0 || !address.is_multiple_of(ALIGN) {
            return None;
        }
        Some(Self(address))
    }

    /// Get the address
    #[inline]
    pub fn as_usize(self) -> usize {
        self.0
    }
}

/// Virtual memory area
#[derive(Debug)]
pub(super) struct VmArea<Platform: PageManagementProvider<ALIGN>, const ALIGN: usize> {
    /// Flags describing the properties of the memory region.
    flags: VmFlags,
    /// Whether this area is backed by a file
    is_file_backed: bool,
    /// For a `VM_SHARED` mapping backed by a real platform shared-memory object (see
    /// [`crate::platform::page_mgmt::PageManagementProvider::create_shared_memory`]): the handle
    /// to re-map (not eagerly copy) during `Vmem::duplicate`, so writes stay visible across
    /// `fork()`. `None` for a private mapping. On a platform that doesn't support real shared
    /// memory, `create_shared_memory` fails when a `VM_SHARED` anonymous mapping is first
    /// created (see `create_pages`), so no `VmArea` on such a platform ever reaches this struct
    /// with `VM_SHARED` set and this field `None` -- that combination cannot occur.
    shared_handle: Option<Platform::SharedMemoryHandle>,
}

// Manual impls since `#[derive(Clone, Copy)]` would incorrectly require `Platform: Clone`/`Copy`
// (derive macros add bounds on every generic parameter, not just the ones actually stored by
// value) -- only `Platform::SharedMemoryHandle` needs to be `Copy`, which the trait already
// guarantees.
impl<Platform: PageManagementProvider<ALIGN>, const ALIGN: usize> Clone
    for VmArea<Platform, ALIGN>
{
    fn clone(&self) -> Self {
        *self
    }
}
impl<Platform: PageManagementProvider<ALIGN>, const ALIGN: usize> Copy for VmArea<Platform, ALIGN> {}
impl<Platform: PageManagementProvider<ALIGN>, const ALIGN: usize> PartialEq
    for VmArea<Platform, ALIGN>
{
    fn eq(&self, other: &Self) -> bool {
        self.flags == other.flags
            && self.is_file_backed == other.is_file_backed
            && self.shared_handle == other.shared_handle
    }
}
impl<Platform: PageManagementProvider<ALIGN>, const ALIGN: usize> Eq for VmArea<Platform, ALIGN> {}

impl<Platform: PageManagementProvider<ALIGN>, const ALIGN: usize> VmArea<Platform, ALIGN> {
    /// Get the [flags](`VmFlags`) of this memory area.
    #[inline]
    pub(super) fn flags(self) -> VmFlags {
        self.flags
    }

    /// Check if this area is backed by a file.
    #[inline]
    pub(super) fn is_file_backed(self) -> bool {
        self.is_file_backed
    }

    /// Create a new private (non-shared) [`VmArea`] with the given flags.
    #[inline]
    pub(super) fn new(flags: VmFlags, is_file_backed: bool) -> Self {
        Self {
            flags,
            is_file_backed,
            shared_handle: None,
        }
    }

    /// Create a new [`VmArea`] backed by a real platform shared-memory object -- see
    /// [`Self::shared_handle`]'s field doc comment.
    #[inline]
    pub(super) fn new_shared(
        flags: VmFlags,
        is_file_backed: bool,
        shared_handle: Platform::SharedMemoryHandle,
    ) -> Self {
        Self {
            flags,
            is_file_backed,
            shared_handle: Some(shared_handle),
        }
    }
}

/// Whether `vma` (covering `range`) is a *private writable data region* of the guest: memory the
/// guest owns exclusively and can legitimately store pointers in, as opposed to code, read-only
/// data, shared mappings, or the stack.
///
/// Recorded per-range by [`Vmem::duplicate`] and surfaced as
/// [`super::AddressRelocations::private_data_ranges`], whose doc comment explains what a consumer
/// uses it for and why the conjunction below -- not any inspection of the contents -- is what
/// makes the classification safe:
///
/// - **writable and not executable**: excludes `.text` and `.rodata`, i.e. every range whose
///   contents are an instruction stream or immutable constants. Rewriting a byte there would
///   corrupt decoded instructions.
/// - **not shared**: a `MAP_SHARED` region is re-mapped rather than copied by [`Vmem::duplicate`],
///   so its contents *are* the parent's live memory; rewriting a pointer there would corrupt a
///   process that is still running.
/// - **not the stack** (`VM_GROWSDOWN`): the stack is deep, dominated by in-progress program data
///   (partially-built strings, locals, arena bytes), and is already covered -- deliberately only
///   within a bounded window above `rsp` -- by the shim's separate stack fixup pass, for reasons
///   that pass documents at length.
///
/// - **not the `brk` heap**: like the stack, the heap is dominated by live allocator-managed
///   payload data (strings, buffers, arbitrary program structures) that a scanning consumer cannot
///   distinguish from the allocator's own bookkeeping pointers by inspecting the range alone.
///   Originally the heap WAS included here on the theory that mallocng's own bookkeeping
///   "genuinely holds pointers that must be relocated" and "never transient stack-style buffers" --
///   disproven live: a real fork()-then-execve() repro (`apk add nodejs` followed by
///   `node --version` in an interactive shell) showed the fork-time fixup pass that consumes this
///   range corrupting the NUL terminator of a live heap-allocated argv string (`"--version\0"`)
///   because its terminator byte shared an 8-byte-aligned scan word with an adjacent, unrelated,
///   genuinely-stale pointer value elsewhere in the same allocation's slack/neighboring bytes --
///   the pass "fixed" the pointer-shaped word and silently destroyed the live string byte(s) that
///   word also happened to cover. This is exactly the same false-positive hazard the stack pass was
///   narrowed to avoid (see [`Vmem`]'s stack-scan-window doc comment in
///   `litebox_shim_linux::syscalls::process::fixup_stale_stack_pointers`), just manifesting in the
///   heap instead of the stack. No real repro has ever required heap coverage specifically (the
///   only repro that motivated adding [`super::AddressRelocations::private_data_ranges`] at all --
///   busybox `ash`'s `.bss` file-stack sentinel -- lives in an ELF's `PF_W` `PT_LOAD` segment, not
///   the heap), so excluding it here closes the argv-corruption bug with no known regression.
///
/// See [`super::AddressRelocations::private_data_ranges`].
/// `(source range, destination base address, was executable in source, is a private data region,
/// was file-backed in source)` for one relocated range -- see [`Vmem::duplicate`].
type DuplicatedRangeInfo = (Range<usize>, usize, bool, bool, bool);

/// One coherent-group reservation made by [`Vmem::duplicate`]: the aligned span it reserved in
/// the SOURCE address space (`source_base..source_base + size`, i.e. `source_group` verbatim) and
/// the aligned base it was actually placed at in the destination (`dest_base`), from which every
/// region within the group is offset identically (see `Vmem::duplicate`'s "COHERENT GROUPS" doc
/// comment on why groups are relocated as one unit rather than per-region).
///
/// # Why this exists (currently unused by any caller)
///
/// This is bookkeeping for a NOT-YET-BUILT process-based `fork()`: today's `fork()` runs the
/// child as a thread in the same host process, so `Vmem::duplicate` only needs each individual
/// region's final destination address (carried in [`DuplicatedRangeInfo`]) -- nothing downstream
/// currently needs to know which regions came from the SAME reservation group, or what that
/// group's own aligned base/size was, once the per-region copy loop finishes.
///
/// A prior investigation (documented in this repo's `FINDINGS.txt`, passes 107-109) root-caused a
/// class of nondeterministic crash to exactly this same-host-process design and designed a real
/// fix: make the child a genuine separate Windows process, `WriteProcessMemory`'d into the SAME
/// addresses the parent used, via forced-address allocation (`VirtualAlloc2` with an explicit
/// address in the child process). That only works at RESERVATION-GROUP granularity -- an
/// already-64KB-aligned base -- not at the granularity of individual guest-visible VMA addresses,
/// which are frequently sub-granularity offsets within one aligned reservation and cannot
/// themselves be independently `VirtualAlloc2`'d at a forced address in a fresh process. A future
/// process-based-fork implementation will need exactly this triple, per group, to replicate each
/// reservation's aligned span in the child process before `WriteProcessMemory`-ing the per-region
/// contents into it. This struct preserves that information (computed internally by the
/// coherent-group-partitioning loop below, but previously discarded once the loop finished)
/// without changing any existing caller's behavior.
#[derive(Debug, Clone)]
#[allow(
    dead_code,
    reason = "consumed by the not-yet-built process-based fork; see doc comment"
)]
pub(super) struct GroupRelocation {
    /// The group's aligned span in the SOURCE address space.
    pub(super) source_group: Range<usize>,
    /// The aligned base address the platform actually placed this group's span at in the
    /// destination (`dest`) address space; every region within `source_group` is offset from
    /// `dest_base` identically to how it was offset from `source_group.start`.
    pub(super) dest_base: usize,
}

/// Return value of [`Vmem::duplicate`]: the existing per-region relocation list plus, additively,
/// the per-group aligned-base bookkeeping described on [`GroupRelocation`]. `group_relocations` is
/// not consulted by any current caller -- see that type's doc comment for why it exists anyway.
pub(super) struct DuplicateOutcome {
    /// One entry per relocated region, in the order regions were processed -- identical in
    /// content and order to what this function returned before `group_relocations` existed.
    pub(super) relocations: Vec<DuplicatedRangeInfo>,
    /// One entry per non-shared coherent group reserved during this call (see this function's
    /// "COHERENT GROUPS" doc comment) -- `VM_SHARED` regions are relocated independently and so
    /// never appear here.
    #[allow(
        dead_code,
        reason = "consumed by the not-yet-built process-based fork; see GroupRelocation's doc comment"
    )]
    pub(super) group_relocations: Vec<GroupRelocation>,
}

/// Whether `range` qualifies for `fork()`-time stale-pointer translation as a loaded ELF's
/// writable data segment or the `brk` heap.
///
/// # Why the heap is included here, unlike a blind whole-heap byte-pattern scan
///
/// An earlier revision of this predicate excluded the heap after a live repro (`apk add nodejs`
/// then `node --version`) showed a heap-scanning pass corrupting an argv string's NUL terminator.
/// That repro's actual cause was a *different*, since-removed heuristic that also existed at the
/// time: a byte-pattern "does this look like a pointer" scan across the WHOLE heap (matching any
/// address-shaped value, live payload or not) rather than a precise translation limited to values
/// that are *provably* stale source-space addresses (`AddressRelocations::translate` only ever
/// rewrites a value that is exactly a captured source-range address -- see its doc comment -- so it
/// cannot mistake an ordinary string byte or small integer for a pointer the way pattern-matching
/// byte VALUES can). Once mis-attributed to "the heap" categorically, the heap was excluded here
/// too -- but that reopened the exact `STATUS_PRIVILEGED_INSTRUCTION` crash this pass exists to
/// fix (a stale post-`fork()` pointer in mallocng's own heap-resident bookkeeping, e.g. busybox
/// `ash`'s file-stack head, reaching `free()` untranslated and tripping mallocng's deliberate
/// alignment `hlt`): live-verified 20/20 (and separately 3/3) crashes on
/// `sh -c "ls /; ls /usr; ls /tmp; ls /bin | head -3"` with the heap excluded, 0/20 with it
/// included, on an otherwise-identical build. The heap is re-admitted here on the same structural,
/// range-membership basis as an ELF's `.data`/`.bss` (private, writable, non-executable,
/// non-stack) -- not a heuristic scan, since `translate`'s exact-match semantics make sweeping it
/// safe regardless of what live payload data shares the scanned range. Note this is deliberately
/// asymmetric with [`super::AddressRelocations::is_in_destination_heap_range`], which still
/// EXCLUDES the heap for a genuinely different, heuristic-shaped consumer (`fork_verify`'s
/// single-step register/write healing, which reasons from live register VALUES during
/// single-stepping rather than `translate`'s exact source-range membership check) -- that
/// exclusion remains correct and is untouched by this change.
pub(super) fn is_private_data_range<Platform: PageManagementProvider<ALIGN>, const ALIGN: usize>(
    vma: &VmArea<Platform, ALIGN>,
) -> bool {
    vma.shared_handle.is_none()
        && !vma.flags().contains(VmFlags::VM_GROWSDOWN)
        && vma.flags().contains(VmFlags::VM_WRITE)
        && !vma.flags().contains(VmFlags::VM_EXEC)
}

/// Virtual Memory Manager
///
/// This struct mantains the virtual memory ranges backed by a memory [backend](PageManagementProvider).
/// Each range needs to be `ALIGN`-aligned.
pub(super) struct Vmem<Platform: PageManagementProvider<ALIGN> + 'static, const ALIGN: usize> {
    /// Memory backend that provides the actual memory.
    pub(super) platform: &'static Platform,
    /// Current program break address.
    pub(super) brk: usize,
    /// Virtual memory areas.
    vmas: RangeMap<usize, VmArea<Platform, ALIGN>>,
}

impl<Platform: PageManagementProvider<ALIGN> + 'static, const ALIGN: usize> Vmem<Platform, ALIGN> {
    pub(super) const STACK_GUARD_GAP: usize = 256 << 12;

    /// Create a new [`Vmem`] instance with the given memory [backend](PageManagementProvider).
    pub(super) fn new(platform: &'static Platform) -> Self {
        Self::new_excluding(platform, core::iter::empty())
    }

    /// Create a new [`Vmem`] instance, treating any of the platform's reported
    /// [`PageManagementProvider::reserved_pages`] that overlap a range in `excluded` as NOT
    /// reserved.
    ///
    /// This exists for `fork()` (see [`Self::duplicate`]): since the platform backend reports
    /// `reserved_pages()` as a snapshot of the whole host process's committed/reserved memory
    /// (there being only one host process backing every guest "process" in this architecture),
    /// a plain [`Self::new`] for a to-be-forked-into child `Vmem` would incorrectly treat the
    /// PARENT's own already-committed guest memory as pre-reserved host state -- even though the
    /// child is meant to claim those exact same addresses as its own independent copy. Passing
    /// the parent's currently-tracked guest ranges as `excluded` here lets the child `Vmem`
    /// legitimately allocate over them.
    pub(super) fn new_excluding(
        platform: &'static Platform,
        excluded: impl Iterator<Item = Range<usize>> + Clone,
    ) -> Self {
        let mut vmem = Self {
            vmas: RangeMap::new(),
            brk: 0,
            platform,
        };
        for each in platform.reserved_pages() {
            assert!(
                each.start % ALIGN == 0 && each.end % ALIGN == 0,
                "Vmem: reserved range is not aligned to {ALIGN} bytes"
            );
            // Subtract every excluded range from `each`, inserting whatever (possibly
            // discontiguous) pieces remain as still-reserved.
            let mut pieces = alloc::vec![each.clone()];
            for excl in excluded.clone() {
                pieces = pieces
                    .into_iter()
                    .flat_map(|p| {
                        let mut out = Vec::new();
                        let overlap_start = p.start.max(excl.start);
                        let overlap_end = p.end.min(excl.end);
                        if overlap_start >= overlap_end {
                            // No overlap with this exclusion.
                            out.push(p);
                        } else {
                            if p.start < overlap_start {
                                out.push(p.start..overlap_start);
                            }
                            if overlap_end < p.end {
                                out.push(overlap_end..p.end);
                            }
                        }
                        out
                    })
                    .collect();
            }
            for piece in pieces {
                if piece.start >= piece.end {
                    continue;
                }
                vmem.vmas.insert(
                    piece,
                    VmArea {
                        flags: VmFlags::empty(),
                        is_file_backed: false,
                        shared_handle: None,
                    },
                );
            }
        }
        vmem
    }

    /// Gets an iterator over all pairs of ([`Range<usize>`], [`VmArea`]),
    /// ordered by key range.
    pub(super) fn iter(&self) -> impl Iterator<Item = (&Range<usize>, &VmArea<Platform, ALIGN>)> {
        self.vmas.iter()
    }

    /// Insert an already-allocated region (e.g., via CoW) without calling the platform allocator.
    ///
    /// Any existing tracked mappings that overlap `range` are silently removed from tracking
    /// (without calling the platform deallocator) before inserting. Use [`Self::overlapping`] to
    /// check for overlap before running this if needed.
    pub(super) fn register_existing_mapping_overwrite(
        &mut self,
        range: PageRange<ALIGN>,
        vma: VmArea<Platform, ALIGN>,
    ) {
        self.vmas.insert(range.into(), vma);
    }

    /// Gets an iterator over all the stored ranges that are
    /// either partially or completely overlapped by the given range.
    pub(super) fn overlapping(
        &self,
        range: Range<usize>,
    ) -> impl DoubleEndedIterator<Item = (&Range<usize>, &VmArea<Platform, ALIGN>)> {
        self.vmas.overlapping(range)
    }

    /// Remove a range from its virtual address space, if all or any of it was present.
    ///
    /// If the range to be removed _partially_ overlaps any ranges, then those ranges will
    /// be contracted to no longer cover the removed range.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the memory region is no longer used by any other.
    pub(super) unsafe fn remove_mapping(
        &mut self,
        range: PageRange<ALIGN>,
    ) -> Result<(), VmemUnmapError> {
        let range: Range<usize> = range.into();
        let is_shared = self
            .vmas
            .overlapping(range.clone())
            .any(|(_, vma)| vma.shared_handle.is_some());
        unsafe {
            if is_shared {
                self.platform
                    .unmap_shared_memory(range.clone())
                    .map_err(|_| VmemUnmapError::UnmapError(DeallocationError::Unaligned))?;
            } else {
                self.platform
                    .deallocate_pages(range.clone())
                    .map_err(VmemUnmapError::UnmapError)?;
            }
        }
        self.vmas.remove(range);
        Ok(())
    }

    /// Reset pages without removing its mapping (similar to Linux `madvise` with
    /// `MADV_DONTNEED` or `MADV_FREE`).
    ///
    /// If `anonymous_only` is true and any part of the range is non‑anonymous (i.e., file‑backed),
    /// returns `Err(VmemResetError::FileBacked)`.
    ///
    /// The current implementation effectively re-inserts the mapping with the same
    /// `VmArea` properties, which will cause the pages to be unmapped and mapped again.
    ///
    /// # Panics
    ///
    /// File-backed mapping is not supported yet.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the memory contents in the affected region are no longer accessed or
    /// relied upon. Any pointers or references to the previous contents become invalid.
    pub(super) unsafe fn reset_pages(
        &mut self,
        range: PageRange<ALIGN>,
        anonymous_only: bool,
    ) -> Result<(), VmemResetError> {
        let range: Range<usize> = range.into();
        // Any unmapped regions in the original range will result in this function returning `DeallocationError::AlreadyUnallocated`
        // while still resetting all of the existing vmas in the range.
        let unmapped_error = self.vmas.gaps(&range).next().is_some();
        let overlapping_ranges: Vec<(Range<usize>, VmArea<Platform, ALIGN>)> = self
            .overlapping(range.clone())
            .map(|(r, vma)| (r.clone(), *vma))
            .collect();
        for (r, vma) in overlapping_ranges {
            if vma.is_file_backed() {
                if anonymous_only {
                    return Err(VmemResetError::FileBacked);
                }
                unimplemented!("resetting file-backed mappings is not supported yet");
            }
            let start = r.start.max(range.start);
            let end = r.end.min(range.end);
            let new_range = PageRange::new(start, end).unwrap();
            unsafe { self.insert_mapping(new_range, vma, false, FixedAddressBehavior::Replace) }
                .expect("failed to reset pages");
        }
        if unmapped_error {
            Err(VmemResetError::AlreadyUnallocated)
        } else {
            Ok(())
        }
    }

    /// Insert a range to its virtual address space.
    ///
    /// If the inserted range partially or completely overlaps any
    /// existing range in the map, then the existing range (or ranges) will be
    /// partially or completely replaced by the inserted range.
    ///
    /// If the inserted range either overlaps or is immediately adjacent
    /// any existing range _mapping to the same value_, then the ranges
    /// will be coalesced into a single contiguous range.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the memory region is not used by any other (i.e., safe
    /// to unmap all overlapping mappings if any).
    pub(super) unsafe fn insert_mapping(
        &mut self,
        suggested_range: PageRange<ALIGN>,
        vma: VmArea<Platform, ALIGN>,
        populate_pages_immediately: bool,
        fixed_address_behavior: FixedAddressBehavior,
    ) -> Result<Platform::RawMutPointer<u8>, AllocationError> {
        let (start, end) = (suggested_range.start, suggested_range.end);
        if start < Platform::TASK_ADDR_MIN {
            return Err(AllocationError::BelowMinAddress);
        }
        if end > Platform::TASK_ADDR_MAX {
            return Err(AllocationError::AboveMaxAddress);
        }
        let platform_fixed_address_behavior = match fixed_address_behavior {
            FixedAddressBehavior::Hint => FixedAddressBehavior::Hint,
            FixedAddressBehavior::NoReplace => {
                // Ensure there are no mappings managed by us.
                if self.vmas.overlaps(&(start..end)) {
                    return Err(AllocationError::AddressInUse);
                }
                FixedAddressBehavior::NoReplace
            }
            FixedAddressBehavior::Replace => {
                if self.vmas.overlaps(&(start..end)) {
                    if self.vmas.gaps(&(start..end)).next().is_some() {
                        // The range is partially overlapping with existing
                        // mappings. If we call into the platform with
                        // `Replace`, then it may overwrite external mappings
                        // that are not managed by us.
                        //
                        // FUTURE: support this case, either by splitting this
                        // into multiple allocate calls or by separating VA
                        // allocation from page backing.
                        return Err(AllocationError::AddressPartiallyInUse);
                    }
                    FixedAddressBehavior::Replace
                } else {
                    // There are no mappings managed by us, so just treat this
                    // as NoReplace.
                    FixedAddressBehavior::NoReplace
                }
            }
        };
        let permissions: u8 = vma
            .flags
            .intersection(VmFlags::VM_ACCESS_FLAGS)
            .bits()
            .try_into()
            .unwrap();
        let max_permissions: u8 = (vma.flags.intersection(VmFlags::VM_MAY_ACCESS_FLAGS).bits()
            >> 4)
            .try_into()
            .unwrap();
        // The `max_permissions` is tracked by `VMem::protect_mapping` and thus doesn't need to be
        // passed to `allocate_pages`.
        let _ = max_permissions;
        let ret = if let Some(shared_handle) = vma.shared_handle {
            // Map the view with the WIDEST permissions the underlying shared-memory object could
            // ever need, then narrow to `vma`'s real permissions via `update_permissions` below
            // -- mirroring how the eager-copy path (below) inserts as READ|WRITE first and
            // protects down after. This matters concretely on Windows: `MapViewOfFile3` fixes a
            // view's MAXIMUM protection at map time (bounded by the section object's own
            // protection ceiling from `create_shared_memory`'s `CreateFileMappingW` call, hence
            // that also requests the widest protection up front), and a later `VirtualProtect`
            // widening it beyond what was granted at map time fails outright -- valid and common
            // for `MAP_SHARED` (e.g. a read-only mapping the guest later `mprotect`s writable, or
            // adds `PROT_EXEC` to for a JIT).
            let widest_permissions = MemoryRegionPermissions::READ
                | MemoryRegionPermissions::WRITE
                | MemoryRegionPermissions::EXEC;
            let dest_ptr = self
                .platform
                .map_shared_memory(
                    shared_handle,
                    suggested_range.into(),
                    widest_permissions,
                    platform_fixed_address_behavior,
                )
                .map_err(|err| match err {
                    SharedMemoryError::AddressInUse => AllocationError::AddressInUseByPlatform,
                    SharedMemoryError::OutOfMemory => AllocationError::OutOfMemory,
                    SharedMemoryError::Unaligned => AllocationError::Unaligned,
                    SharedMemoryError::UnsupportedByPlatform => {
                        // Unreachable in practice: a `shared_handle` only ever exists if
                        // `create_shared_memory` (from the same platform) already succeeded.
                        AllocationError::OutOfMemory
                    }
                })?;
            let actual_permissions = MemoryRegionPermissions::from_bits(permissions).unwrap();
            if actual_permissions != widest_permissions {
                let mapped_range =
                    dest_ptr.as_usize()..(dest_ptr.as_usize() + suggested_range.len());
                unsafe {
                    self.platform
                        .update_permissions(mapped_range, actual_permissions)
                }
                .expect("failed to narrow newly-mapped shared memory permissions");
            }
            dest_ptr
        } else {
            self.platform
                .allocate_pages(
                    suggested_range.into(),
                    MemoryRegionPermissions::from_bits(permissions).unwrap(),
                    vma.flags.contains(VmFlags::VM_GROWSDOWN),
                    populate_pages_immediately,
                    platform_fixed_address_behavior,
                )
                .map_err(|err| match err {
                    AllocationError::AddressInUse => AllocationError::AddressInUseByPlatform,
                    other => other,
                })?
        };
        let new_start = ret.as_usize();
        let new_end = new_start + suggested_range.len();
        self.vmas.insert(new_start..new_end, vma);
        debug_assert!(new_start >= Platform::TASK_ADDR_MIN);
        debug_assert!(new_end <= Platform::TASK_ADDR_MAX);
        Ok(ret)
    }

    /// Duplicate every guest-tracked mapping (i.e. everything `insert_mapping` has recorded
    /// since construction, excluding the platform's host-reserved ranges pre-populated by
    /// [`Self::new`]) into `dest`, eagerly copying the contents of each region.
    ///
    /// This is the `fork()` address-space duplication primitive: `dest` must be a freshly
    /// constructed, otherwise-empty [`Vmem`] for the same `Platform`. On success `dest` has an
    /// independent copy of every byte currently readable in `self`; subsequent writes to either
    /// address space do not affect the other.
    ///
    /// `VM_SHARED` mappings backed by a real platform shared-memory object (see
    /// [`PageManagementProvider::create_shared_memory`]) are re-mapped at `dest` rather than
    /// eagerly copied, so writes through either mapping stay visible to the other -- genuine
    /// `fork()` + `MAP_SHARED` sharing. On a platform that doesn't support real shared memory,
    /// no `VmArea` ever carries a shared handle in the first place (see
    /// [`crate::platform::page_mgmt::PageManagementProvider::create_shared_memory`]'s default
    /// body), so this path is simply never taken there.
    ///
    /// # Known deviation from real `fork()`: addresses are NOT preserved
    ///
    /// Real Linux `fork()` gives the child a separate address space with the SAME virtual
    /// addresses as the parent (via separate page tables), so any pointer valid in the parent
    /// remains valid, unchanged, in the child. On a platform backend with only one real host
    /// address space for the whole litebox process (true of `litebox_platform_windows_userland`:
    /// there is no Windows primitive to give two logical "processes" the same VirtualAlloc2
    /// addresses while both remain live in the same host process), that guarantee cannot be
    /// upheld -- the host OS will refuse a second `VirtualAlloc2` at an address the parent
    /// already committed. This function therefore lets the platform pick a fresh address for
    /// each duplicated region ([`FixedAddressBehavior::Hint`]), meaning **any raw pointer value
    /// stored in the copied memory that pointed into the OLD address space will be a dangling /
    /// wrong address in the child**. This does not affect the dominant real-world usage pattern
    /// (`fork()` immediately followed by `execve()`, e.g. every external command a shell runs),
    /// since `execve()` discards the entire address space and loads a fresh one anyway. It DOES
    /// mean a guest program that forks and then continues running in the child without exec,
    /// relying on pointers computed before the fork, can misbehave. This is a genuine
    /// architectural limitation of the single-host-address-space design, not a bug to silently
    /// paper over -- see `fork-child-address-relocation-limitation` for further context.
    ///
    /// # Safety
    ///
    /// The caller must ensure no other code is concurrently mutating the memory tracked by
    /// `self` for the duration of this call (e.g. other threads of the same process must be
    /// stopped), since the copy reads each region's live contents non-atomically.
    ///
    /// Returns the list of `(source_range, dest_base_address)` relocations that were actually
    /// applied, in the order regions were processed, so the caller can translate any pointer
    /// (e.g. captured CPU register state like `rsp`) that referenced the source address space
    /// into the corresponding address in `dest`.
    pub(super) unsafe fn duplicate<DestPlatform>(
        &self,
        dest: &mut Vmem<DestPlatform, ALIGN>,
    ) -> Result<DuplicateOutcome, VmemDuplicateError>
    where
        DestPlatform: PageManagementProvider<ALIGN, SharedMemoryHandle = Platform::SharedMemoryHandle>
            + 'static,
    {
        // Each entry's third field: whether the range was executable (`VM_EXEC`) in `self` (the
        // source). Threaded through to `AddressRelocations::is_executable_range` so a consumer
        // that must scan destination memory for stale pointers can exclude code pages -- see that
        // method's doc comment. The fourth: whether it is a private writable data region (see
        // `is_private_data_range` and `AddressRelocations::private_data_ranges`).
        let mut relocations: Vec<DuplicatedRangeInfo> = Vec::new();
        // Collect first: `insert_mapping` on `dest` only touches `dest.vmas`, but we still avoid
        // holding a borrow of `self.vmas` across it for clarity and to allow future parallel
        // copying without restructuring this loop.
        //
        // Ranges with empty flags are the platform's host-reserved placeholders inserted by
        // `Self::new`/`Self::new_excluding` (not real guest mappings, and not necessarily even
        // readable) -- `dest` gets its own copy of those from its own construction, so skip them
        // here rather than trying to copy host-runtime memory that is none of the guest's
        // business.
        let regions: Vec<(Range<usize>, VmArea<Platform, ALIGN>)> = self
            .vmas
            .iter()
            .filter(|(_, vma)| !vma.flags.is_empty())
            .map(|(r, vma)| (r.clone(), *vma))
            .collect();

        // Non-shared regions must be relocated in COHERENT GROUPS, not independently: real guest
        // code (any dynamically-linked or PIE binary, which is the overwhelming majority) uses
        // RIP-relative addressing across ELF segments -- e.g. a `call *offset(%rip)` in `.text`
        // reading a function pointer out of `.got`, which are always mapped as SEPARATE regions
        // (one per `PT_LOAD` segment / `mmap` call) but at a FIXED relative distance from each
        // other, guaranteed by the original single coherent virtual address layout the linker
        // computed. Relocating each region independently (as this function used to do, in the
        // same style still used below for `PROT_NONE` guard pages and `VM_SHARED` regions, where
        // no such cross-region relationship exists) would let two regions of the SAME loaded
        // object land at DIFFERENT relative offsets in the child, silently corrupting every
        // RIP-relative reference that crosses a region boundary -- observed as a NULL-pointer
        // crash jumping through a GOT-style table whose entries read back wrong after `fork()`.
        //
        // The fix: partition regions into contiguity-based groups (adjacent-or-near regions,
        // i.e. the segments of one loaded ELF image, separated by no more than
        // `MAX_INTRA_GROUP_GAP`) rather than one single span covering the WHOLE address space --
        // a single global span would also force the guest's stack (placed far from the ELF's own
        // low-address segments, with no RIP-relative relationship to them at all) into the same
        // reservation, requiring an absurdly large, likely-unsatisfiable allocation. Each group is
        // reserved as ONE contiguous span at a single freshly-chosen base address, then every
        // region within it is placed at `group_new_base + (region.start - group_min_start)` --
        // preserving every pairwise relative offset within the group exactly, the same guarantee
        // real Linux `fork()` gets for free by giving the child the SAME virtual addresses as the
        // parent (see this function's "Known deviation" doc section on why that specific
        // guarantee isn't available here). Regions in DIFFERENT groups (e.g. the stack vs. the
        // main ELF image) have no such relationship and may land anywhere independently.
        let max_intra_group_gap: usize = 16 * ALIGN;
        let mut sorted_non_shared: Vec<Range<usize>> = regions
            .iter()
            .filter(|(_, vma)| vma.shared_handle.is_none())
            .map(|(r, _)| r.clone())
            .collect();
        sorted_non_shared.sort_by_key(|r| r.start);
        let mut groups: Vec<Range<usize>> = Vec::new();
        for r in sorted_non_shared {
            match groups.last_mut() {
                Some(last) if r.start <= last.end.saturating_add(max_intra_group_gap) => {
                    last.end = last.end.max(r.end);
                }
                _ => groups.push(r),
            }
        }
        // For each source address, the `(group_source_base, group_dest_base)` of the group it
        // falls in -- looked up per-region in the main loop below via a linear scan (`groups` is
        // small: one entry per ELF image / stack / independent mmap cluster, not per region).
        let mut group_bases: Vec<(Range<usize>, usize)> = Vec::with_capacity(groups.len());
        for group in &groups {
            let span_page_range = PageRange::<ALIGN>::new(group.start, group.end)
                .ok_or(VmemDuplicateError::UnAligned)?;
            // A pure address-space reservation: populate nothing yet (each region below performs
            // its own real copy-and-populate into a `Replace`-mode sub-range of this group), and
            // use empty flags since this placeholder `VmArea` is never queried directly -- only
            // individual regions placed within it via `Replace` below are tracked in `dest.vmas`
            // (each `insert_mapping` call replaces this placeholder's tracking for its own
            // sub-range).
            let placeholder_vma = VmArea::<DestPlatform, ALIGN>::new(VmFlags::empty(), false);
            let base_ptr = unsafe {
                dest.insert_mapping(
                    span_page_range,
                    placeholder_vma,
                    /* populate_pages_immediately */ false,
                    FixedAddressBehavior::Hint,
                )
            }
            .map_err(VmemDuplicateError::Allocation)?;
            group_bases.push((group.clone(), base_ptr.as_usize()));
        }
        // Preserved additively for `DuplicateOutcome::group_relocations` -- see that field's doc
        // comment. Built from the same `group_bases` entries used by the per-region placement loop
        // below; not consulted by anything in that loop itself.
        let group_relocations: Vec<GroupRelocation> = group_bases
            .iter()
            .map(|(group, dest_base)| GroupRelocation {
                source_group: group.clone(),
                dest_base: *dest_base,
            })
            .collect();

        // Tracks the relocation of whichever source region contains `self.brk`, if any, so the
        // destination's brk can point at the corresponding relocated address rather than a
        // stale one. `self.brk == 0` means brk was never initialized on the source; leave
        // `dest.brk` as `0` in that case (its own default) rather than fabricating a value.
        let mut brk_relocation: Option<(Range<usize>, usize)> = None;

        for (range, vma) in regions {
            let page_range = PageRange::<ALIGN>::new(range.start, range.end)
                .ok_or(VmemDuplicateError::UnAligned)?;
            let (_, length) = page_range.start_and_length();

            // Reconstruct for `DestPlatform` -- `vma` (from `self`, a `Vmem<Platform, ALIGN>`)
            // can't be used directly on `dest` (a `Vmem<DestPlatform, ALIGN>`) even though the
            // two platforms share the same `SharedMemoryHandle` type (the `where` bound above),
            // since `VmArea<Platform, ALIGN>` and `VmArea<DestPlatform, ALIGN>` are distinct
            // types to the compiler.
            let dest_vma = match vma.shared_handle {
                Some(handle) => VmArea::new_shared(vma.flags, vma.is_file_backed, handle),
                None => VmArea::new(vma.flags, vma.is_file_backed),
            };

            if vma.shared_handle.is_some() {
                // `VM_SHARED` backed by a real platform shared-memory object: re-map the SAME
                // handle at a (possibly different) destination address instead of eagerly
                // copying bytes, so writes through either mapping stay visible to the other --
                // this is what makes `fork()` + `MAP_SHARED` genuinely share memory rather than
                // just starting with identical initial contents. Independently relocated (not
                // placed within `non_shared_span`): a shared object's mapped address has no
                // compile-time relationship to any other region's RIP-relative code, unlike the
                // ELF-segment case `non_shared_span` exists for.
                let dest_ptr = unsafe {
                    dest.insert_mapping(
                        page_range,
                        dest_vma,
                        /* populate_pages_immediately */ false,
                        FixedAddressBehavior::Hint,
                    )
                }
                .map_err(VmemDuplicateError::Allocation)?;
                if self.brk != 0 && range.contains(&self.brk) {
                    brk_relocation = Some((range.clone(), dest_ptr.as_usize()));
                }
                relocations.push((
                    range.clone(),
                    dest_ptr.as_usize(),
                    vma.flags.contains(VmFlags::VM_EXEC),
                    is_private_data_range(&vma),
                    vma.is_file_backed(),
                ));
                continue;
            }

            // Every non-shared region was already reserved as part of its group in `group_bases`
            // above -- place it at its fixed position within that group's span (preserving the
            // exact relative offset every other region in the same group has from it) rather than
            // letting the platform pick a fresh, unrelated address per region.
            let (group_source_base, group_dest_base) = group_bases
                .iter()
                .find(|(group, _)| group.start <= range.start && range.end <= group.end)
                .map_or_else(
                    || unreachable!("every non-shared region falls within a group by construction"),
                    |(group, dest_base)| (group.start, *dest_base),
                );
            let forced_addr = group_dest_base + (range.start - group_source_base);
            let forced_page_range =
                PageRange::<ALIGN>::new(forced_addr, forced_addr + length.as_usize())
                    .ok_or(VmemDuplicateError::UnAligned)?;

            if vma.flags.intersection(VmFlags::VM_ACCESS_FLAGS).is_empty() {
                // `PROT_NONE` region (e.g. a stack guard page): genuinely unreadable right now,
                // by design. There is no content to copy -- just create an equivalent
                // inaccessible mapping at its fixed position within its group's span.
                let dest_ptr = unsafe {
                    dest.insert_mapping(
                        forced_page_range,
                        dest_vma,
                        /* populate_pages_immediately */ false,
                        FixedAddressBehavior::Replace,
                    )
                }
                .map_err(VmemDuplicateError::Allocation)?;
                if self.brk != 0 && range.contains(&self.brk) {
                    brk_relocation = Some((range.clone(), dest_ptr.as_usize()));
                }
                relocations.push((
                    range.clone(),
                    dest_ptr.as_usize(),
                    vma.flags.contains(VmFlags::VM_EXEC),
                    is_private_data_range(&vma),
                    vma.is_file_backed(),
                ));
                continue;
            }

            // Read the full source region's live bytes up front so the write side can populate
            // the destination pages via a single `op` callback (matching how `create_pages`
            // wants its initializer).
            let source_ptr = Platform::RawConstPointer::<u8>::from_usize(range.start);
            let source_bytes = source_ptr
                .to_owned_slice(length.as_usize())
                .ok_or(VmemDuplicateError::SourceUnreadable)?;

            // Insert as READ|WRITE first regardless of the source's real permissions: a
            // read-only source region (e.g. an ELF text/rodata segment) would otherwise reject
            // the write below before we ever get a chance to populate it. `create_pages`
            // elsewhere in this module solves the identical problem via its `before_perms` /
            // `after_perms` split; do the same here by protecting down to `vma`'s real flags
            // only after the copy succeeds.
            let writable_vma = VmArea::new(
                (vma.flags & !VmFlags::VM_ACCESS_FLAGS) | VmFlags::VM_READ | VmFlags::VM_WRITE,
                vma.is_file_backed,
            );
            // Place at this region's fixed position within `non_shared_span` (reserved above),
            // preserving its exact relative offset from every other non-shared region -- see
            // `non_shared_span`'s doc comment for why this must NOT be independently relocated.
            let dest_ptr = unsafe {
                dest.insert_mapping(
                    forced_page_range,
                    writable_vma,
                    /* populate_pages_immediately */ true,
                    FixedAddressBehavior::Replace,
                )
            }
            .map_err(VmemDuplicateError::Allocation)?;
            dest_ptr
                .write_slice_at_offset(0, &source_bytes)
                .ok_or(VmemDuplicateError::DestUnwritable)?;

            if self.brk != 0 && range.contains(&self.brk) {
                brk_relocation = Some((range.clone(), dest_ptr.as_usize()));
            }
            relocations.push((
                range.clone(),
                dest_ptr.as_usize(),
                // The SOURCE's real flags (`vma.flags`), not `writable_vma`'s temporary
                // READ|WRITE-forced flags used only to populate this mapping above.
                vma.flags.contains(VmFlags::VM_EXEC),
                is_private_data_range(&vma),
                vma.is_file_backed(),
            ));

            if vma.flags.intersection(VmFlags::VM_ACCESS_FLAGS)
                != writable_vma.flags().intersection(VmFlags::VM_ACCESS_FLAGS)
            {
                let dest_range = PageRange::<ALIGN>::new(
                    dest_ptr.as_usize(),
                    dest_ptr.as_usize() + length.as_usize(),
                )
                .ok_or(VmemDuplicateError::UnAligned)?;
                unsafe { dest.protect_mapping(dest_range, vma.flags.into()) }
                    .map_err(|_| VmemDuplicateError::DestUnwritable)?;
            }
        }

        dest.brk = match brk_relocation {
            Some((source_range, dest_start)) => dest_start + (self.brk - source_range.start),
            None => self.brk,
        };
        Ok(DuplicateOutcome {
            relocations,
            group_relocations,
        })
    }

    /// Create a new mapping in the virtual address space.
    ///
    /// `suggested_address` is the hint address for where to create the pages if it is not `None`.
    /// Otherwise, let the kernel choose an available memory region.
    ///
    /// `length` is the size of the pages to be created.
    ///
    /// Set `flags` to control options such as fixed address, stack, and populate pages.
    ///
    /// Return `Some(new_addr)` if the mapping is created successfully.
    /// The returned address is `ALIGN`-aligned.
    ///
    /// # Fixed Address Behavior
    ///
    /// - [`CreatePagesFlags::FIXED_ADDR`] alone: Forces allocation at the exact address, replacing
    ///   any existing overlapping mappings. Caller must ensure overlapping mappings are not in use.
    /// - [`CreatePagesFlags::FIXED_ADDR`] with [`CreatePagesFlags::NOREPLACE`]: Forces allocation at
    ///   the exact address, but fails with [`AllocationError::AddressInUse`] if any part of the
    ///   range is already mapped. This is safe to use without checking for existing mappings first.
    /// - Without [`CreatePagesFlags::FIXED_ADDR`], the address is treated as a hint.
    ///
    /// Note: `NOREPLACE` error responses (`AddressInUse` / `EEXIST`) can be used to probe memory
    /// layout. This matches Linux kernel behavior for `MAP_FIXED_NOREPLACE`.
    ///
    /// # Safety
    ///
    /// When using [`CreatePagesFlags::FIXED_ADDR`] without [`CreatePagesFlags::NOREPLACE`], the
    /// caller must ensure any overlapping mappings are not used by any other code, as they will be
    /// unmapped.
    pub(super) unsafe fn create_mapping(
        &mut self,
        suggested_address: Option<NonZeroAddress<ALIGN>>,
        length: NonZeroPageSize<ALIGN>,
        vma: VmArea<Platform, ALIGN>,
        flags: CreatePagesFlags,
    ) -> Result<Platform::RawMutPointer<u8>, AllocationError> {
        let total_length = (length
            + if flags.contains(CreatePagesFlags::ENSURE_SPACE_AFTER) {
                DEFAULT_RESERVED_SPACE_SIZE
            } else {
                0
            })
        .unwrap();
        let new_addr = self
            .get_unmmaped_area(
                suggested_address,
                total_length,
                flags.contains(CreatePagesFlags::FIXED_ADDR),
                vma.flags.contains(VmFlags::VM_GROWSDOWN),
            )
            .ok_or(AllocationError::OutOfMemory)?;
        // new_addr must be ALIGN aligned
        let new_range = PageRange::new(new_addr, new_addr + length.as_usize()).unwrap();
        unsafe {
            self.insert_mapping(
                new_range,
                vma,
                flags.contains(CreatePagesFlags::POPULATE_PAGES_IMMEDIATELY),
                if flags.contains(CreatePagesFlags::FIXED_ADDR) {
                    if flags.contains(CreatePagesFlags::NOREPLACE) {
                        FixedAddressBehavior::NoReplace
                    } else {
                        FixedAddressBehavior::Replace
                    }
                } else {
                    FixedAddressBehavior::Hint
                },
            )
        }
    }

    /// Resize a range in the virtual address space.
    /// Shrink the range if it is larger than `new_size`.
    /// Enlarge the range if it is smaller than `new_size` and will not overlap with
    /// next mapping after the expansion.
    ///
    /// It fails if it resizes more than one mapping or needs to split the current mapping
    /// (due to enlarging).
    ///
    /// See <https://elixir.bootlin.com/linux/v5.19.17/source/mm/mremap.c#L886> for reference.
    ///
    /// # Safety
    ///
    /// If it shrinks, the caller must ensure that the unmapped memory region is not used by any other.
    pub(super) unsafe fn resize_mapping(
        &mut self,
        range: PageRange<ALIGN>,
        new_size: NonZeroPageSize<ALIGN>,
    ) -> Result<(), VmemResizeError> {
        let range = range.start..range.end;
        // `cur_range` contains `range.start`
        let (cur_range, cur_vma) = self
            .vmas
            .get_key_value(&range.start)
            .ok_or(VmemResizeError::NotExist(range.start))?;

        let new_end = range.start + new_size.as_usize();
        match new_end.cmp(&range.end) {
            core::cmp::Ordering::Equal => {
                // no change
                return Ok(());
            }
            core::cmp::Ordering::Less => {
                // shrink
                let range = PageRange::new(new_end, range.end).unwrap();
                unsafe { self.remove_mapping(range) }.unwrap();
                return Ok(());
            }
            core::cmp::Ordering::Greater => {}
        }

        // grow
        if range.end > cur_range.end {
            // we can't remap across vm area boundaries
            return Err(VmemResizeError::InvalidAddr {
                range: cur_range.clone(),
                addr: range.end,
            });
        }

        if range.end == cur_range.end {
            // expand the current range
            let r = range.end..new_end;
            if self.vmas.overlaps(&r) {
                return Err(VmemResizeError::RangeOccupied(r));
            }
            if cur_vma.is_file_backed() {
                unimplemented!("file-backed mapping expansion is not supported yet");
            }
            let range = PageRange::new(range.end, new_end).unwrap();
            // Try to extend the mapping. Although we checked that there are no
            // litebox mappings in this range, this may fail if there are
            // platform mappings in the way.
            match unsafe {
                self.insert_mapping(range, *cur_vma, false, FixedAddressBehavior::NoReplace)
            } {
                Ok(_) => {}
                Err(AllocationError::OutOfMemory) => return Err(VmemResizeError::OutOfMemory),
                Err(
                    AllocationError::AddressInUse
                    | AllocationError::AddressInUseByPlatform
                    | AllocationError::AddressPartiallyInUse,
                ) => return Err(VmemResizeError::RangeOccupied(range.into())),
                Err(
                    AllocationError::Unaligned
                    | AllocationError::BelowMinAddress
                    | AllocationError::AboveMaxAddress,
                ) => unreachable!(),
            }
            return Ok(());
        }

        // has to split the current range and move it to somewhere else
        Err(VmemResizeError::RangeOccupied(range.end..cur_range.end))
    }

    /// Move a range from `old_range` to `suggested_new_range`.
    /// Use it together with [`Vmem::resize_mapping`] to achieve `mremap`.
    ///
    /// The `suggested_new_range.start` is used as a hint for the new address.
    /// If it is zero, kernel will choose a new suitable address freely.
    ///
    /// Returns `Some(new_addr)` if the range is moved successfully
    /// Otherwise, returns `None`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the given `range` is safe to be unmapped.
    ///
    /// # Panics
    ///
    /// Panics if the size of `suggested_new_range` is smaller than the size of `old_range`.
    /// Panics if the `old_range` is not covered by exactly one mapping.
    pub(super) unsafe fn move_mappings(
        &mut self,
        old_range: PageRange<ALIGN>,
        suggested_new_address: Option<NonZeroAddress<ALIGN>>,
        new_size: NonZeroPageSize<ALIGN>,
    ) -> Result<Platform::RawMutPointer<u8>, VmemMoveError> {
        assert!(new_size.as_usize() >= old_range.len());

        // Check if the given range is covered by exactly one mapping
        let (cur_range, vma) = self
            .vmas
            .get_key_value(&old_range.start)
            .expect("VMEM: range not found");
        assert!(cur_range.contains(&(old_range.end - 1)));

        if vma.is_file_backed() {
            unimplemented!("file-backed mapping move is not supported yet");
        }
        let new_addr = self
            .get_unmmaped_area(
                suggested_new_address,
                new_size,
                false,
                vma.flags.contains(VmFlags::VM_GROWSDOWN),
            )
            .ok_or(VmemMoveError::OutOfMemory)?;
        let new_range = PageRange::<ALIGN>::new(new_addr, new_addr + new_size.as_usize()).unwrap();
        let new_addr = unsafe {
            self.platform
                .remap_pages(old_range.into(), new_range.into(), vma.flags.into())
        }
        .map_err(VmemMoveError::RemapError)?;

        let new_start = new_addr.as_usize();
        let new_end = new_start + new_size.as_usize();
        self.vmas.insert(new_start..new_end, *vma);
        self.vmas.remove(old_range.into());
        Ok(new_addr)
    }

    /// Change the permissions ([`VmFlags::VM_ACCESS_FLAGS`]) of a range in the virtual address space.
    ///
    /// See <https://elixir.bootlin.com/linux/v5.19.17/source/mm/mprotect.c#L617> for reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure it is safe to change the permissions of the given range, e.g., no more
    /// write access to the range if it is changed to read-only.
    pub(super) unsafe fn protect_mapping(
        &mut self,
        range: PageRange<ALIGN>,
        permissions: MemoryRegionPermissions,
    ) -> Result<(), VmemProtectError> {
        // `MemoryRegionPermissions` is a subset of `VmFlags` and we only change the access flags
        let flags =
            VmFlags::from_bits(u32::from(permissions.bits())).unwrap() & VmFlags::VM_ACCESS_FLAGS;
        let range = range.start..range.end;
        let mut mappings_to_change = Vec::new();
        for (r, vma) in self.vmas.overlapping(range.clone()) {
            mappings_to_change.push((r.start, r.end, *vma));
        }
        if mappings_to_change.is_empty() {
            return Err(VmemProtectError::InvalidRange(range));
        }

        for (start, end, vma) in mappings_to_change {
            if vma.flags & VmFlags::VM_ACCESS_FLAGS == flags {
                continue;
            }
            // flags >> 4 shift VM_MAY% in place of VM_%
            // turning on VM_% requires VM_MAY%
            if (!(vma.flags.bits() >> 4) & flags.bits()) & VmFlags::VM_ACCESS_FLAGS.bits() != 0 {
                return Err(VmemProtectError::NoAccess {
                    old: vma.flags,
                    new: flags,
                });
            }

            self.vmas.remove(start..end);
            let intersection = range.start.max(start)..range.end.min(end);
            // split r into three parts: before, intersection, and after
            let before = start..intersection.start;
            let after = intersection.end..end;

            let new_flags = (vma.flags & !VmFlags::VM_ACCESS_FLAGS) | flags;
            // `intersection` is page aligned.
            unsafe {
                self.platform
                    .update_permissions(intersection.clone(), permissions)
            }
            .map_err(|e| {
                // restore the original mapping
                self.vmas.insert(start..end, vma);
                VmemProtectError::ProtectError(e)
            })?;

            self.vmas.insert(
                intersection,
                VmArea {
                    flags: new_flags,
                    is_file_backed: vma.is_file_backed,
                    shared_handle: vma.shared_handle,
                },
            );
            if !before.is_empty() {
                self.vmas.insert(before, vma);
            }
            if !after.is_empty() {
                self.vmas.insert(after, vma);
            }
        }

        Ok(())
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
    /// `perm` is the permissions to set for the created pages.
    ///
    /// # Safety
    ///
    /// Note that if the suggested address is given and [`CreatePagesFlags::FIXED_ADDR`] is set,
    /// the kernel uses it directly without checking if it is available, causing overlapping
    /// mappings to be unmapped. Caller must ensure any overlapping mappings are not used by any other.
    ///
    /// Also, caller must ensure flags are set correctly.
    pub(super) unsafe fn create_pages(
        &mut self,
        suggested_new_address: Option<NonZeroAddress<ALIGN>>,
        length: NonZeroPageSize<ALIGN>,
        flags: CreatePagesFlags,
        perms: MemoryRegionPermissions,
    ) -> Result<Platform::RawMutPointer<u8>, MappingError> {
        let shared = flags.contains(CreatePagesFlags::SHARED);
        let file_backed = flags.contains(CreatePagesFlags::MAP_FILE);
        let vm_flags = VmFlags::from(perms)
            | VmFlags::may_flags_for_mapping(shared, file_backed)
            | if flags.contains(CreatePagesFlags::IS_STACK) {
                VmFlags::VM_GROWSDOWN
            } else {
                VmFlags::empty()
            };
        // Anonymous `MAP_SHARED`: back it with a real platform shared-memory object (see
        // `PageManagementProvider::create_shared_memory`'s doc comment) so it survives `fork()`
        // as genuinely shared memory (see `Vmem::duplicate`) rather than being eagerly copied.
        // File-backed shared mappings are handled separately by the caller (currently rejected
        // upfront for writable mappings -- see `litebox_shim_linux`'s `sys_mmap`) and don't need
        // this, since re-opening/re-mmap'ing the same file in the child already gives the same
        // sharing semantics without a platform-specific handle.
        let vma = if shared && !file_backed {
            let shared_handle = self
                .platform
                .create_shared_memory(length.as_usize())
                .map_err(|_| MappingError::MapError(AllocationError::OutOfMemory))?;
            VmArea::new_shared(vm_flags, file_backed, shared_handle)
        } else {
            VmArea::new(vm_flags, file_backed)
        };
        unsafe { self.create_mapping(suggested_new_address, length, vma, flags) }
            .map_err(MappingError::MapError)
    }

    /// Get the memory permissions of a given address range.
    ///
    /// `page_range` specifies the range of pages to check the memory permissions.
    /// This function returns `MemoryRegionPermissions` only if the range is valid.
    pub(super) fn get_memory_permissions(
        &self,
        page_range: PageRange<ALIGN>,
    ) -> Option<MemoryRegionPermissions> {
        let (range_start, range_end) = (page_range.start, page_range.end);
        let range: core::ops::Range<usize> = page_range.into();
        if let Some(iter) = self.overlapping(range).next() {
            if iter.0.start > range_start || iter.0.end < range_end {
                // partial overlap implies that the given range contains unmapped pages or
                // consists of memory pages with different permissions.
                return None;
            }
            let vmflags = iter.1.flags();
            Some(vmflags.into())
        } else {
            None
        }
    }

    /*================================Internal Functions================================ */

    /// Get an unmapped area in the virtual address space.
    /// `suggested_range` and `fixed_addr` are the hint address and MAP_FIXED flag respectively,
    /// similar to how `mmap` works.
    ///
    /// Returns `None` if no area found. Otherwise, returns the start address of a page-aligned area.
    fn get_unmmaped_area(
        &self,
        suggested_address: Option<NonZeroAddress<ALIGN>>,
        length: NonZeroPageSize<ALIGN>,
        fixed_addr: bool,
        is_growsdown: bool,
    ) -> Option<usize> {
        let size = length.as_usize();
        if size > Platform::TASK_ADDR_MAX {
            return None;
        }
        if let Some(suggested_address) = suggested_address {
            if (Platform::TASK_ADDR_MAX - size) < suggested_address.0 {
                return None;
            }
            if fixed_addr
                || !self
                    .vmas
                    .overlaps(&(suggested_address.0..(suggested_address.0 + size)))
            {
                return Some(suggested_address.0);
            }
        } else if fixed_addr {
            // MAP_FIXED with addr=0: return 0 so insert_mapping rejects it
            // via the TASK_ADDR_MIN check (BelowMinAddress → EPERM).
            return Some(0);
        }

        // top down
        // 1. check [last_end, TASK_SIZE_MAX)
        let (low_limit, high_limit) = (
            Platform::TASK_ADDR_MIN,
            Platform::TASK_ADDR_MAX - length.as_usize(),
        );
        debug_assert_eq!(Platform::TASK_ADDR_MIN % ALIGN, 0);
        debug_assert_eq!(Platform::TASK_ADDR_MAX % ALIGN, 0);
        let last_end = self.vmas.last_range_value().map_or(low_limit, |r| r.0.end);
        if last_end <= high_limit {
            // A growsdown (stack) region must keep a guard gap below whatever is already
            // mapped above it -- gap #2 below already reserves this in the OTHER direction
            // (a later placement avoiding landing too close above an EXISTING stack's
            // downward growth), but that logic only runs once this fast path is skipped.
            // Without this, a stack placed here can end up directly, contiguously adjacent
            // to an already-mapped region (e.g. `ld.so`, itself placed top-down earlier) with
            // zero separation, only guarded by luck of the arithmetic not aligning exactly.
            if is_growsdown && last_end > low_limit {
                let gapped_high_limit = high_limit.checked_sub(Self::STACK_GUARD_GAP)?;
                if gapped_high_limit >= last_end {
                    return Some(gapped_high_limit);
                }
            } else {
                return Some(high_limit);
            }
        }

        // 2. check gaps between ranges
        for (r, flags) in self.vmas.iter().rev() {
            let gap_below_r = if flags.flags.contains(VmFlags::VM_GROWSDOWN) {
                // If it is a stack, we need to leave enough space for the stack to grow downwards.
                Self::STACK_GUARD_GAP << 1
            } else {
                0
            };
            // If the NEW region is itself a stack, it also needs a guard gap between its own
            // top and `r` (whatever is already mapped directly above it) -- symmetric to the
            // case above, and to the fast-path reservation a few lines up.
            let gap_above_new = if is_growsdown {
                Self::STACK_GUARD_GAP
            } else {
                0
            };
            let start = r.start.checked_sub(size + gap_below_r.max(gap_above_new))?;
            if start < low_limit {
                return None;
            }
            if start > high_limit {
                // Note we may have pre-allocated memory that are higher than `TASK_ADDR_MAX`
                // (See [`Vmem::new`]) and thus `start` may be larger than `high_limit`.
                continue;
            }
            if !self.vmas.overlaps(&(start..start + size)) {
                return Some(start);
            }
        }

        None
    }
}

/// Error for `Vmem::duplicate` (see [`crate::mm::PageManager::duplicate`], its public wrapper)
#[derive(Error, Debug)]
pub enum VmemDuplicateError {
    #[error("arg is not aligned")]
    UnAligned,
    #[error("failed to read source mapping contents")]
    SourceUnreadable,
    #[error("failed to write duplicated contents into the destination mapping")]
    DestUnwritable,
    #[error("failed to allocate destination mapping: {0}")]
    Allocation(#[from] AllocationError),
}

/// Error for removing mappings
#[derive(Error, Debug)]
pub enum VmemUnmapError {
    #[error("arg is not aligned")]
    UnAligned,
    #[error("failed to unmap pages: {0}")]
    UnmapError(#[from] crate::platform::page_mgmt::DeallocationError),
}

/// Error for resetting pages
#[derive(Error, Debug)]
pub enum VmemResetError {
    #[error("arg is not aligned")]
    UnAligned,
    #[error("provided range contains unallocated pages")]
    AlreadyUnallocated,
    #[error("reset file-backed mapping")]
    FileBacked,
}

/// Error for [`Vmem::resize_mapping`]
#[derive(Error, Debug)]
pub(super) enum VmemResizeError {
    #[error("no mapping containing the address {0:?}")]
    NotExist(usize),
    #[error("invalid address {addr:?} exceeds range {range:?}")]
    InvalidAddr { range: Range<usize>, addr: usize },
    #[error("range {0:?} is already (partially) occupied")]
    RangeOccupied(Range<usize>),
    #[error("out of memory")]
    OutOfMemory,
}

/// Error for moving mappings
#[derive(Error, Debug)]
pub enum VmemMoveError {
    #[error("arg is not aligned")]
    UnAligned,
    #[error("out of memory")]
    OutOfMemory,
    #[error("remap failed: {0}")]
    RemapError(#[from] crate::platform::page_mgmt::RemapError),
}

/// Error for protecting mappings
#[derive(Error, Debug)]
pub enum VmemProtectError {
    #[error("the range {0:?} is not aligned")]
    UnAligned(Range<usize>),
    #[error("the range {0:?} has no mapping memory")]
    InvalidRange(Range<usize>),
    #[error("failed to change permissions from {old:?} to {new:?}")]
    NoAccess { old: VmFlags, new: VmFlags },
    #[error("mprotect failed: {0}")]
    ProtectError(#[from] crate::platform::page_mgmt::PermissionUpdateError),
}

/// Error for creating mappings
#[non_exhaustive]
#[derive(Error, Debug)]
pub enum MappingError {
    #[error("arg is not aligned")]
    UnAligned,
    #[error("not enough memory")]
    OutOfMemory,

    // Errors from mapping a file
    #[error("bad file descriptor: {0}")]
    BadFD(i32),
    #[error("file descriptor does not point to a file")]
    NotAFile,
    #[error("file not open for reading")]
    NotForReading,
    #[error("I/O error while reading file contents into the mapping")]
    Io,

    #[error("mapping failed: {0}")]
    MapError(#[from] crate::platform::page_mgmt::AllocationError),
}

/// Enable [`super::PageManager`] to handle page faults if its platform implements this trait
pub trait VmemPageFaultHandler {
    /// Handle a page fault for the given address.
    ///
    /// # Safety
    ///
    /// This should only be called from the kernel page fault handler.
    unsafe fn handle_page_fault(
        &self,
        fault_addr: usize,
        flags: VmFlags,
        error_code: u64,
    ) -> Result<(), PageFaultError>;

    /// Check if it has access to the fault address.
    fn access_error(error_code: u64, flags: VmFlags) -> bool;
}

/// Error for handling page fault
#[derive(Error, Debug)]
pub enum PageFaultError {
    #[error("no access: {0}")]
    AccessError(&'static str),
    #[error("allocation failed")]
    AllocationFailed,
    #[error("given page is part of an already mapped huge page")]
    HugePage,
}
