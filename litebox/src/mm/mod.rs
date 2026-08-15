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
    /// Parallel to `ranges`: whether the corresponding range was executable (`VM_EXEC`) in the
    /// SOURCE address space at the moment of duplication. Populated alongside `ranges` in
    /// [`PageManager::duplicate`]; queried via [`Self::is_executable_range`] so a consumer that
    /// must scan destination memory for stale pointers (e.g. a fork-time proactive stack-slot
    /// fixup pass) can exclude code pages the same way it can exclude the heap -- see that
    /// consumer's own doc comment for why blindly scanning-and-rewriting ordinary program data
    /// (as opposed to the narrow, deliberately-bounded set of slots that pass exists to fix) is a
    /// false-positive hazard, which applies with even more severity to a scan that can rewrite
    /// individual bytes of a decoded instruction stream into a privileged or otherwise undefined
    /// opcode.
    executable: Vec<bool>,
    /// Parallel to `ranges`: whether the corresponding range is a *private writable data region*
    /// of the guest -- see [`Self::private_data_ranges`], which this exists to back.
    private_data: Vec<bool>,
    /// Parallel to `ranges`: whether the corresponding range was file-backed (`mmap`ped from a
    /// real file, e.g. an ELF's `PT_LOAD` segment reached via `MAP_PRIVATE`) in the SOURCE address
    /// space at the moment of duplication, as opposed to anonymous. Used together with
    /// `private_data` by [`Self::is_in_destination_heap_range`] to identify "heap-like" memory:
    /// anonymous, private, writable, non-executable, non-stack data the guest's allocator (not the
    /// loader) owns -- see that method's doc comment for why file-backed private data (an ELF's
    /// `.data`/`.got`/`.bss`) must stay excluded from this classification even though it also
    /// satisfies every other `is_private_data_range` condition.
    is_file_backed: Vec<bool>,
    /// The source (parent) process's program break at the moment of duplication, i.e. the current
    /// upper bound of its heap (`brk`-allocated) region -- `0` if `set_initial_brk` was never
    /// called (no heap exists yet). By construction (`PageManager::brk`'s `create_pages` call),
    /// the heap VMA's tracked range always ends exactly here, which is what lets
    /// [`Self::heap_range`] identify it precisely rather than by heuristic.
    heap_top: usize,
    /// One `(source_group, dest_base)` pair per non-shared coherent reservation group duplicated
    /// during this call -- the same bookkeeping as `linux::GroupRelocation`, re-exposed here as a
    /// plain tuple so callers outside the `mm` module (currently: a diagnostic, `LITEBOX_DIAG_
    /// PROCESS_FORK_SPAWN`-gated probe in `do_clone` proving out a future process-based `fork()`;
    /// see `scratchpad/jqrepro/FINDINGS.txt` passes 107-110) can read each group's aligned base
    /// without needing `linux`-module visibility. Unlike `ranges` (per individual guest-visible
    /// VMA, frequently a sub-64KB-granularity offset within a shared reservation), each entry here
    /// is independently `VirtualAlloc2`-at-a-forced-address-able -- see `linux::GroupRelocation`'s
    /// doc comment for why that distinction matters.
    group_relocations: Vec<(Range<usize>, usize)>,
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

    /// Returns the `(source reservation-group span, destination group base)` pairs -- see this
    /// struct's `group_relocations` field doc comment for why this is a coarser granularity than
    /// [`Self::ranges`] and who currently consumes it.
    #[must_use]
    pub fn group_relocations(&self) -> &[(Range<usize>, usize)] {
        &self.group_relocations
    }

    /// Returns whether the range at `ranges()[index]` was executable (`VM_EXEC`) in the source
    /// address space at the moment of duplication.
    ///
    /// Panics if `index` is out of bounds for `ranges()` -- the two slices are always the same
    /// length by construction (`PageManager::duplicate` pushes to both in lockstep).
    #[must_use]
    pub fn is_executable_range(&self, index: usize) -> bool {
        self.executable[index]
    }

    /// Returns whether `addr` falls within a DESTINATION range that was executable (`VM_EXEC`) in
    /// the SOURCE address space at the moment of duplication -- i.e. `addr` is inside the child's
    /// own relocated copy of guest code.
    ///
    /// A write landing here is never legitimate guest behavior: ordinary program code does not
    /// self-modify, so any write an instruction's *own* operand computes into this range is itself
    /// evidence of a stale/corrupted address-forming register, exactly the class of bug this
    /// module exists to catch -- distinct from [`Self::is_in_source`], which only catches a write
    /// through an address that is *still untranslated* (literally in a source range); a
    /// corrupted-but-already-"valid"-looking destination address (e.g. off by a small amount from
    /// a genuinely translated one) would slip past that check while still being exactly as
    /// dangerous, since it corrupts the child's own instruction stream instead of the parent's
    /// live state.
    #[must_use]
    pub fn is_in_destination_executable_range(&self, addr: usize) -> bool {
        self.ranges
            .iter()
            .enumerate()
            .any(|(i, (source_range, dest_base))| {
                self.executable[i] && (*dest_base..dest_base + source_range.len()).contains(&addr)
            })
    }

    /// Returns the destination-space range of the source process's heap (`brk`-allocated region)
    /// at the moment of duplication, if it has one (`set_initial_brk` was called and at least one
    /// successful `brk()` growth has happened, so a heap VMA actually exists and was duplicated).
    ///
    /// Identified precisely, not heuristically: `PageManager::brk`'s `create_pages` call always
    /// creates the heap VMA to end exactly at the current program break, so the (unique) tracked
    /// source range whose end equals the program break captured at duplication time -- *is* the
    /// heap, by construction of how that range came to exist. Used by
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

    /// Returns whether `addr` falls within any DESTINATION-space "heap-like" range: the `brk`
    /// heap (see [`Self::heap_range`]) *or* any anonymous (not file-backed), private, writable,
    /// non-executable, non-stack range -- i.e. exactly [`Self::private_data_ranges`]'s
    /// classification minus file-backed regions (an ELF's `.data`/`.got`/`.bss`, reached via
    /// `MAP_PRIVATE` over a file, which must stay eligible for healing below).
    ///
    /// Used by consumers that must exclude heap-like memory from a broad DESTINATION-space memory
    /// scan or heal, for the same reason [`Self::private_data_ranges`] excludes the `brk` heap
    /// from the proactive fork-time fixup pass: it is dominated by live, allocator-managed payload
    /// data (argv copies, strings, arbitrary buffers, allocator metadata) that a scan cannot
    /// distinguish from a genuine stale pointer by inspecting a single 8-byte word's bit pattern
    /// alone. Confirmed live: litebox's post-`fork()` single-step verifier (`fork_verify`, in
    /// `litebox_platform_windows_userland`) was found healing a DESTINATION-range heap slot
    /// holding a live argv string's tail bytes during ash's `fork()`-then-`execve()` window,
    /// corrupting the string's NUL terminator -- the same false-positive hazard as the (already
    /// heap-excluded) `.data`/`.bss` fixup pass, just reached through the verifier's indirect-
    /// call/jmp-target healing instead.
    ///
    /// The anonymous-range half of this predicate closes a second, later-discovered instance of
    /// the identical hazard: `heap_range` alone only identifies the single VMA `PageManager::brk`
    /// grows, which is `None` (or excludes everything) for a process whose allocator never calls
    /// `brk` at all -- musl's mallocng, the allocator every repro in this investigation actually
    /// runs under, allocates its slab groups via anonymous `mmap`, not `sbrk`. For such a process
    /// `heap_top == 0`, so the old `heap_range`-only check was a permanent no-op and every one of
    /// mallocng's live slab-group/meta-object allocations was fully exposed to case (3)/(4)
    /// healing -- confirmed live via `python3 -c "import pty; pty.spawn(['/bin/echo','x'])"`
    /// (`os.fork()` under mallocng): a case (3) heal fired on a `call [mem]` target slot
    /// milliseconds before the child crashed in mallocng's own `get_meta` back-link integrity
    /// check (`cmp [rax+0x10],rcx; je +1; hlt`, `rax` holding `0x65`, a small integer -- not a
    /// pointer in either address space -- consistent with a heap-metadata slot having been
    /// overwritten by an unrelated healing write rather than a genuine stale pointer ever having
    /// been there).
    #[must_use]
    pub fn is_in_destination_heap_range(&self, addr: usize) -> bool {
        if self.heap_range().is_some_and(|(source_range, dest_base)| {
            (dest_base..dest_base + source_range.len()).contains(&addr)
        }) {
            return true;
        }
        self.ranges
            .iter()
            .zip(self.private_data.iter().zip(&self.is_file_backed))
            .any(
                |((source_range, dest_base), (is_private_data, is_file_backed))| {
                    *is_private_data
                        && !*is_file_backed
                        && (*dest_base..dest_base + source_range.len()).contains(&addr)
                },
            )
    }

    /// Returns the `(source range, destination base)` pairs of every tracked range classified as
    /// a *private writable data region* of the guest at the moment of duplication: private
    /// (non-`MAP_SHARED`), writable, non-executable, and not the stack -- i.e. a loaded ELF
    /// image's `.data`/`.got`/`.bss`-style segment or an anonymous private mapping, never code,
    /// read-only data, a shared mapping, the stack, or (see `linux::is_private_data_range`'s doc
    /// comment) the `brk` heap. That conjunction (not any inspection of contents) is what makes it
    /// precise enough to scan and rewrite unconditionally, unlike a heuristic whole-region sweep.
    ///
    /// Used by a fork-time fixup pass that must translate stale, untranslated SOURCE-space
    /// pointers a loaded ELF image's `.data`/`.got`/`.bss` segment stored before duplication (e.g.
    /// an `R_X86_64_RELATIVE`/`RELR`-initialized global pointing at another symbol in the same
    /// image) into their DESTINATION equivalents.
    pub fn private_data_ranges(&self) -> impl Iterator<Item = (Range<usize>, usize)> + '_ {
        self.ranges
            .iter()
            .zip(&self.private_data)
            .filter(|(_, is_private)| **is_private)
            .map(|((source_range, dest_base), _)| (source_range.clone(), *dest_base))
    }

    /// Returns the `(source range, destination base)` pairs [`Self::private_data_ranges`] would,
    /// *excluding* any anonymous (non-file-backed) range other than the `brk` heap itself.
    ///
    /// **Not currently used by `fixup_stale_elf_data_pointers`** (tried and reverted three times --
    /// see below). Kept, tested, and documented for a future attempt.
    ///
    /// The first revert (see "Why wiring this into `fixup_stale_elf_data_pointers` was reverted"
    /// below) traded the crash this method fixes for a livelock in `fork_verify`'s reactive
    /// healer, caused by case (2c)/(4) being unable to recognize a stale value reached through
    /// more than one memory load, or through a load followed by pointer arithmetic. That gap was
    /// closed by `fork_verify::LastLoad`, which traces a chain of constant-offset arithmetic
    /// applied to a value read from a tracked slot, not just an exact single-load match.
    ///
    /// Re-attempting the narrowing with that fix in place surfaced a SECOND, independent
    /// soundness gap instead of the livelock: case (2c) was not restricted to call/jmp targets the
    /// way case (3)/(4) are (see that case's own doc comment in `fork_verify.rs`), so on a slot
    /// this narrower scan no longer proactively covers, it could "heal" (mistranslate) an ordinary
    /// non-pointer, non-16-byte-aligned tagged value that merely coincides with a tracked source
    /// range -- observed live producing a bogus, still-misaligned address subsequently passed to
    /// `free()`, tripping its alignment assertion. That gap was closed too: case (2c) now
    /// additionally requires the loaded value to satisfy `fork_verify::MIN_POINTER_ALIGN` (16-byte
    /// alignment, matching every allocation mallocng's own allocator ever hands out) before
    /// healing -- see that constant's doc comment for the full soundness argument.
    ///
    /// A THIRD attempt, with both of those fixes in place, traded the first crash for a genuine,
    /// deterministic, unbounded livelock on a DIFFERENT repro than the first revert's (the pty
    /// smoke test, `python3 -c "import pty; pty.spawn(['/bin/echo','x'])"`): `LITEBOX_VEH_TRACE=1`
    /// showed `rip` cycling through the exact same ~120-instruction sequence forever, never
    /// terminating even after 2+ minutes (the broad sweep lets the identical repro finish in well
    /// under a second). Both the multi-hop chain and the alignment gate are real, sound,
    /// independently verified improvements to case (2c) -- but neither, together or alone, closes
    /// the deeper multi-indirection gap this narrowing exposes: some base pointer this loop
    /// reloads is reached through more indirection (or a different traversal shape entirely) than
    /// one memory-load chain can trace, and the broad sweep's proactive coverage of it is load-
    /// bearing. Landing this narrowing again requires a genuinely deeper reactive trace (not just
    /// one more hop, as the constant-offset chain added) or a different, still-narrower proactive
    /// strategy that keeps covering whatever this loop's base pointer needs.
    ///
    /// [`Self::private_data_ranges`] deliberately includes the `brk` heap alongside a loaded ELF's
    /// `.data`/`.got`/`.bss` -- see that method's doc comment and `fixup_stale_elf_data_pointers`'s
    /// (in `litebox_shim_linux`) for why: excluding the heap there reopened a real crash where
    /// busybox `ash`'s `brk`-resident file-stack sentinel needed the same proactive translation an
    /// ELF's `.data` segment gets. But `private_data_ranges` also, necessarily, includes every
    /// *other* anonymous private-writable mapping -- in particular every one of an mmap-based
    /// allocator's own slab/arena groups (musl's mallocng allocates this way exclusively; it never
    /// calls `brk` at all). Those are not ELF globals or a `brk`-grown heap: they are dense,
    /// allocator-owned metadata -- bitmasks, size classes, indices -- packed with small integers
    /// that can coincidentally fall inside some *other* tracked range's numeric span (ranges can
    /// be megabytes wide), which `translate`'s range-membership check (not a value-identity check,
    /// despite `private_data_ranges`' doc comment describing it as "precise") then "translates" and
    /// overwrites in place, corrupting live allocator bookkeeping.
    ///
    /// Confirmed live: `python3 -c "import pty; pty.spawn(['/bin/echo','x'])"` (`os.fork()` under
    /// mallocng, which never calls `brk`) crashed deterministically in mallocng's own `get_meta`
    /// back-link integrity check (`cmp [rax+0x10],rcx; je +1; hlt`) moments after `fork()`, with
    /// the checked slot holding a small integer (`0x65`) rather than a pointer in either address
    /// space -- consistent with exactly this class of miscategorized overwrite, not a genuine
    /// stale pointer.
    ///
    /// # Why wiring this into `fixup_stale_elf_data_pointers` was reverted
    ///
    /// Narrowing that pass to use this method DOES eliminate the crash above, but trades it for a
    /// worse failure: a genuine infinite livelock in `fork_verify`'s reactive single-step healer,
    /// observed live on the identical repro -- two instructions (a `Cmp` and a `Movzx`) alternate
    /// forever, `fork_verify`'s case (2b)/(2c) each retrying and (for 2c) attempting to heal the
    /// memory slot the stale register was loaded from, but never converging. The proactive sweep
    /// this method would narrow was, it turns out, ALSO incidentally pre-healing a heap-resident
    /// slot that case (2b)/(2c)'s narrower, single-slot-at-a-time healing cannot reach on its own
    /// (most likely a base pointer reached through more than one level of indirection, or advanced
    /// by pointer arithmetic rather than a single fixed load) -- removing that incidental coverage
    /// exposes a second, previously-latent gap in the reactive healer that a future pass should
    /// close (e.g. extending case (2b)/(2c) to trace back further than one memory load, or a
    /// different proactive strategy that is narrower than a full range sweep but still covers
    /// whatever this loop's base pointer needs) BEFORE re-attempting this narrowing.
    pub fn private_data_ranges_excluding_anonymous_mmap(
        &self,
    ) -> impl Iterator<Item = (Range<usize>, usize)> + '_ {
        let heap_range = self.heap_range();
        self.ranges
            .iter()
            .zip(self.private_data.iter().zip(&self.is_file_backed))
            .filter(
                move |((source_range, dest_base), (is_private, is_file_backed))| {
                    **is_private
                        && (**is_file_backed
                            || heap_range.as_ref().is_some_and(|(hr, hd)| {
                                hr.start == source_range.start
                                    && hr.end == source_range.end
                                    && hd == dest_base
                            }))
                },
            )
            .map(|((source_range, dest_base), _)| (source_range.clone(), *dest_base))
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
        let linux::DuplicateOutcome {
            relocations,
            group_relocations,
        } = unsafe { source_vmem.duplicate(&mut dest_vmem) }?;
        let mut ranges = Vec::with_capacity(relocations.len());
        let mut executable = Vec::with_capacity(relocations.len());
        let mut private_data = Vec::with_capacity(relocations.len());
        let mut is_file_backed = Vec::with_capacity(relocations.len());
        for (range, dest_base, is_executable, is_private_data, was_file_backed) in relocations {
            ranges.push((range, dest_base));
            executable.push(is_executable);
            private_data.push(is_private_data);
            is_file_backed.push(was_file_backed);
        }
        // Re-exposed as a plain tuple (see `AddressRelocations::group_relocations`'s doc
        // comment) -- consumed only by the `LITEBOX_DIAG_PROCESS_FORK_SPAWN`-gated diagnostic in
        // `do_clone`, not by today's same-host-process fork path itself.
        let group_relocations = group_relocations
            .into_iter()
            .map(|g| (g.source_group, g.dest_base))
            .collect();
        Ok((
            Self {
                vmem: RwLock::new(dest_vmem),
            },
            AddressRelocations {
                ranges,
                executable,
                private_data,
                is_file_backed,
                heap_top,
                group_relocations,
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
            executable: alloc::vec![false, false, false],
            private_data: alloc::vec![false, true, false],
            is_file_backed: alloc::vec![false, false, false],
            heap_top: 0x1010_0000,
            group_relocations: alloc::vec![],
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
            executable: alloc::vec![false],
            private_data: alloc::vec![false],
            is_file_backed: alloc::vec![false],
            heap_top: 0,
            group_relocations: alloc::vec![],
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
            executable: alloc::vec![false],
            private_data: alloc::vec![false],
            is_file_backed: alloc::vec![false],
            heap_top: 0x1234_5678,
            group_relocations: alloc::vec![],
        };

        assert_eq!(relocations.heap_range(), None);
    }

    /// `private_data_ranges` must return exactly the ranges flagged `true` in the parallel
    /// `private_data` vec (source range and destination base preserved verbatim), in original
    /// order, skipping every non-private-data range (e.g. code, stack, or a shared mapping) --
    /// this is the primitive `fixup_stale_elf_data_pointers` (in `litebox_shim_linux`) relies on
    /// to scan precisely, so a wrong filter here would either miss a genuine stale pointer or
    /// rewrite a range it must not touch.
    #[test]
    fn private_data_ranges_returns_only_flagged_ranges_in_order() {
        let relocations = AddressRelocations {
            ranges: alloc::vec![
                (0x1000_0000..0x1000_1000, 0x2000_0000), // code: excluded
                (0x1000_1000..0x1000_2000, 0x2000_1000), // .data: included
                (0x7000_0000..0x7080_0000, 0x9000_0000), // stack: excluded
                (0x1000_2000..0x1000_3000, 0x2000_2000), // anon private mapping: included
            ],
            executable: alloc::vec![true, false, false, false],
            private_data: alloc::vec![false, true, false, true],
            // The `.data` segment is file-backed (mmap'd from the ELF); the anon private mapping
            // is not -- see the new `is_in_destination_heap_range` test below, which relies on
            // exactly this distinction.
            is_file_backed: alloc::vec![true, true, false, false],
            heap_top: 0x1000_3000,
            group_relocations: alloc::vec![],
        };

        let got: alloc::vec::Vec<_> = relocations.private_data_ranges().collect();
        assert_eq!(
            got,
            alloc::vec![
                (0x1000_1000..0x1000_2000, 0x2000_1000),
                (0x1000_2000..0x1000_3000, 0x2000_2000),
            ]
        );
    }

    /// Regression coverage for the mallocng/`os.fork()` `hlt` crash this investigation
    /// root-caused: a process whose allocator never calls `brk` at all (musl's mallocng, which
    /// allocates its slab groups via anonymous `mmap`) has `heap_top == 0`, so `heap_range()`
    /// alone can never identify any range as heap-like -- `is_in_destination_heap_range` must
    /// still recognize an anonymous, private, writable, non-executable range as heap-like via the
    /// `private_data`/`is_file_backed` classification, while continuing to treat a file-backed
    /// private range (an ELF's `.data`/`.got`/`.bss`) as NOT heap-like, since `fork_verify`'s
    /// case (3)/(4) healing must still be able to patch stale GOT/PLT-style pointers living there.
    #[test]
    fn is_in_destination_heap_range_covers_anonymous_private_data_even_without_a_brk_heap() {
        let relocations = AddressRelocations {
            ranges: alloc::vec![
                (0x1000_0000..0x1000_1000, 0x2000_0000), // .data (file-backed): NOT heap-like
                (0x3000_0000..0x3000_2000, 0x4000_0000), // mallocng slab group (anon mmap): heap-like
                (0x7000_0000..0x7080_0000, 0x9000_0000), // stack: NOT heap-like
            ],
            executable: alloc::vec![false, false, false],
            private_data: alloc::vec![true, true, false],
            is_file_backed: alloc::vec![true, false, false],
            heap_top: 0, // mallocng never calls brk: no brk-heap VMA exists at all.
            group_relocations: alloc::vec![],
        };

        assert!(
            !relocations.is_in_destination_heap_range(0x2000_0500),
            "a file-backed private range (.data/.got/.bss) must stay eligible for healing"
        );
        assert!(
            relocations.is_in_destination_heap_range(0x4000_1000),
            "an anonymous private-writable range must be treated as heap-like even with no brk heap"
        );
        assert!(!relocations.is_in_destination_heap_range(0x9000_0100));
    }

    /// Regression coverage for the same mallocng/`os.fork()` bug from the consuming side:
    /// `private_data_ranges_excluding_anonymous_mmap` must still include a file-backed private
    /// range (ELF `.data`/`.got`/`.bss`, needed for the ordinary RELR/RELATIVE-relocation fixup
    /// case) and the `brk` heap itself (needed for the `ash` file-stack-sentinel case this pass
    /// was originally written for), while excluding every OTHER anonymous private-writable range
    /// -- i.e. an mmap-based allocator's own slab/arena groups, which is what `private_data_ranges`
    /// alone (no exclusion) would still incorrectly sweep.
    #[test]
    fn private_data_ranges_excluding_anonymous_mmap_keeps_brk_heap_and_elf_data_only() {
        let relocations = AddressRelocations {
            ranges: alloc::vec![
                (0x1000_0000..0x1000_1000, 0x2000_0000), // .data (file-backed): included
                (0x1000_3000..0x1000_5000, 0x2000_3000), // brk heap: included
                (0x3000_0000..0x3000_2000, 0x4000_0000), // mallocng slab group (anon, non-brk): excluded
                (0x7000_0000..0x7080_0000, 0x9000_0000), // stack: excluded (not private_data at all)
            ],
            executable: alloc::vec![false, false, false, false],
            private_data: alloc::vec![true, true, true, false],
            is_file_backed: alloc::vec![true, false, false, false],
            heap_top: 0x1000_5000,
            group_relocations: alloc::vec![],
        };

        let got: alloc::vec::Vec<_> = relocations
            .private_data_ranges_excluding_anonymous_mmap()
            .collect();
        assert_eq!(
            got,
            alloc::vec![
                (0x1000_0000..0x1000_1000, 0x2000_0000),
                (0x1000_3000..0x1000_5000, 0x2000_3000),
            ],
            "must keep the file-backed ELF-data range and the brk heap range, excluding the \
             anonymous non-brk mmap range"
        );
    }
}
