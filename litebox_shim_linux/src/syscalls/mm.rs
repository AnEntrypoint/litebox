// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Implementation of memory management related syscalls, eg., `mmap`, `munmap`, etc.
//! Most of these syscalls which are not backed by files are implemented in [`litebox_common_linux::mm`].

use alloc::collections::{BTreeMap, BTreeSet};
use litebox::{
    mm::linux::{MappingError, PAGE_SIZE, PageRange},
    platform::{
        PageManagementProvider, RawConstPointer, RawMutPointer,
        page_mgmt::{FixedAddressBehavior, MemoryRegionPermissions},
    },
};
use litebox_common_linux::{MRemapFlags, MapFlags, ProtFlags, errno::Errno};

use crate::ShimFS;
use crate::ShimPlatform;
use crate::Task;
use crate::UserPtrMut;
use litebox::utils::TruncateExt as _;
#[cfg(target_arch = "x86_64")]
use object::elf::{ET_DYN, FileHeader64, PT_LOAD, ProgramHeader64};
#[cfg(target_arch = "x86_64")]
use object::endian::LittleEndian;

/// Whether the copy-on-write file-mapping fast path is allowed to run. Default **off**.
///
/// Off by default because the path is currently INCORRECT on Windows, and separately is not
/// buying anything. Both halves are measured, not assumed:
///
/// * **Incorrect.** A `MapViewOfFile3` CoW view can only be destroyed whole -- Windows has no
///   partial-unmap for a mapped view -- so when the guest `mmap(MAP_FIXED)`s a sub-range of a
///   view (exactly what `ld.so` does: one whole-library view, then a fixed sub-mmap per
///   `PT_LOAD`), the flanking remainder either side of that sub-range dies with it. Nothing in
///   this codebase can reconstruct those flanks as equivalent CoW mappings, because no
///   guest-address -> file/offset tracking exists (`VmArea` records only `is_file_backed: bool`),
///   so the best available recovery re-creates them as anonymous ZERO-FILL pages. That is
///   memory-safe but silently lossy, and for a shared library the lost bytes are real content.
///   Measured directly: with this path enabled, `python3 -c "import pixelflux"` fails with
///   `ImportError: Error relocating .../libplacebo-...so: dovi_rpu_get_header: symbol not found`
///   even though `libdovi-...so` is present and correct in the same directory -- the symbol is
///   missing because the pages holding it were zero-filled. With this path disabled, the same
///   import succeeds, as does `import pcmflux`. That is selkies' entire capture/encode layer, and
///   therefore the webtop's whole video path.
/// * **Not buying anything.** See AGENTS.md, "Windows CoW-mmap performance": the optimisation was
///   investigated to a conclusion and found to have "zero practical effect" on real tar-packed
///   execs, because `MapViewOfFile3` requires 64 KiB file-offset alignment while real ELF
///   `PT_LOAD` file offsets are only page-aligned. It succeeds mainly on deliberately
///   realignment-padded images -- which is precisely where it now does damage.
///
/// Set `LITEBOX_COW_MMAP=1` to opt back in (e.g. to continue the alignment/CoW investigation the
/// open PRD rows describe). Nothing else about the CoW implementation is changed by this flag; it
/// only decides whether the fast path is attempted at all.
static COW_MMAP_ENABLED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Enable (or disable) the copy-on-write file-mapping fast path. See [`COW_MMAP_ENABLED`].
///
/// The shim is `no_std` and cannot read an environment variable itself, so the runner forwards
/// `LITEBOX_COW_MMAP` on its behalf -- the same arrangement `set_mapping_guard_gap_disabled`
/// already uses for `LITEBOX_NO_MAPPING_GUARD_GAP`.
pub fn set_cow_mmap_enabled(enabled: bool) {
    COW_MMAP_ENABLED.store(enabled, core::sync::atomic::Ordering::Relaxed);
}

/// Whether the copy-on-write file-mapping fast path may be attempted.
fn cow_mmap_enabled() -> bool {
    COW_MMAP_ENABLED.load(core::sync::atomic::Ordering::Relaxed)
}

/// One System V shared-memory segment.
///
/// **This is NOT a single host address valid in every guest process.** An earlier revision of
/// this type assumed "litebox runs every guest process inside ONE real host address space",
/// which was true of the original thread-based fork model (every guest "process" a thread inside
/// one Windows process) but is false the moment `LITEBOX_PROCESS_FORK=1`'s cross-process fork
/// path is taken: a guest process attaching a segment it did not create is then a genuinely
/// separate Windows process, with its own private address space, in which the CREATOR's
/// `addr` was never mapped to anything at all. `shmat` handing that raw numeric value back
/// produced a real, wild, unmapped pointer in the attaching process -- read by whatever copy
/// eventually touched it, an ordinary `memmove`/`memcpy` -- root-caused to this exact defect via
/// the live `LITEBOX_DIAG_FATALDUMP=1` register capture of the second Xvfb SIGSEGV (51st pass):
/// `rsi` (the wild source pointer) never moved with Xvfb's own ASLR base across independent
/// boots, which is exactly what a value copied verbatim out of the shared `GlobalState` table
/// (rather than derived from this process's own, ASLR'd, mmap placement) looks like from the
/// crash site. The X11 MIT-SHM extension makes this reachable on every real desktop boot: a
/// client creates a segment and tells the SERVER (Xvfb, never fork-related to the client) its id
/// over the wire; the server's own `shmat` is exactly the non-creator attach this bug breaks.
///
/// Fixed (51st pass) by keying each segment to a NAMED platform shared-memory object
/// (`PageManagementProvider::create_named_shared_memory`, `Local\litebox_sysvshm_<shmid>`)
/// instead of a bare address: every attacher, including the creator's own first `shmat`, now
/// opens that name and establishes a REAL mapping in ITS OWN address space via the existing
/// `map_existing_shared_pages` machinery (same primitive `syscalls::file`'s memfd/`wl_shm`
/// bridging already uses) -- see `Task::sys_shmat`. The resulting address is per-process (real
/// Linux `shmat` addresses are never guaranteed identical across processes either), so this type
/// no longer carries one at all.
#[derive(Clone, Copy)]
pub(crate) struct SysvShmSegment {
    /// Size in bytes, rounded up to a page.
    size: usize,
    /// The `key` this segment was created for, or `IPC_PRIVATE` (0).
    key: i32,
    /// Number of live `shmat` attachments across every process combined.
    attaches: usize,
    /// Set by `shmctl(IPC_RMID)`. Real Linux keeps a removed segment alive until the last
    /// detach, and so does this.
    removed: bool,
}

/// Realistic upper bound on simultaneously live SysV shm segments in one guest session (X11's
/// MIT-SHM extension allocates one per client-side pixmap/framebuffer pool, plus Xvfb's own
/// `-shmem` framebuffer) -- sized generously, never grown, same discipline as
/// `syscalls::unix::UNIX_ADDR_PRESENCE_CAPACITY`.
pub(crate) const MAX_SYSV_SHM_SEGMENTS: usize = 128;

#[derive(Clone, Copy)]
struct ShmSlot {
    shmid: i32,
    segment: SysvShmSegment,
}

/// All System V shared-memory segments, plus the key -> id index `shmget` needs.
///
/// A fixed-size, pointer-free slot array -- deliberately NOT a `BTreeMap` (the type this field
/// used before the 2026-09-18 systematic `GlobalState`-field audit). `GlobalState::sysv_shm` is
/// genuinely, correctly meant to be shared across the whole cross-process-fork family (see its
/// own doc comment: "any process that knows the key or id can attach", exactly what X11's
/// MIT-SHM extension and Xvfb's own `-shmem` framebuffer rely on for real cross-process content
/// sharing), so unlike `unix_addr_table`/`fifo_registry` (fixed the same 2026-09-18 audit pass by
/// shadowing them as per-process-private on `GlobalStateHandle` instead) this table cannot simply
/// be made per-process -- two DIFFERENT guest processes' `shmget(same key)` genuinely must
/// resolve to the same segment. A `BTreeMap`'s heap-allocated nodes are the SAME defect class
/// already fixed a dozen times over elsewhere in this crate (see `GlobalStateHandle`'s own doc
/// comment): an attaching cross-process-fork child's copy of the root pointer is the first
/// creator's, meaningless in its own address space. `SysvShmSegment` itself is already fully
/// `Copy`/pointer-free (a handful of `usize`/`i32`/`bool` fields, no `Vec`/`Box`/`Arc`), so a
/// flat array of `Option<ShmSlot>` needs no `unsafe` and no second `SharedKernelStateProvider`
/// slot -- it inherits whatever cross-process sharing `GlobalState` itself already gets for free,
/// the same reasoning `syscalls::unix::SharedUnixAddrPresenceTable`'s own doc comment gives.
/// `shmid` values are NOT slot indices (real Linux `shmid`s are opaque, monotonically-issued via
/// `GlobalState::next_shmid`, and this table must tolerate holes as segments are removed), so
/// every lookup is a linear scan over [`MAX_SYSV_SHM_SEGMENTS`] slots -- cheap, since these
/// syscalls (`shmget`/`shmat`/`shmdt`/`shmctl`) are rare compared to the data-plane operations
/// that actually move pixels.
pub(crate) struct SysvShmTable {
    slots: [Option<ShmSlot>; MAX_SYSV_SHM_SEGMENTS],
}

impl SysvShmTable {
    pub(crate) fn new() -> Self {
        Self {
            slots: [None; MAX_SYSV_SHM_SEGMENTS],
        }
    }

    fn index_of_id(&self, shmid: i32) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| matches!(slot, Some(s) if s.shmid == shmid))
    }

    fn index_of_key(&self, key: i32) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| matches!(slot, Some(s) if s.segment.key == key))
    }

    fn get_mut(&mut self, shmid: i32) -> Option<&mut SysvShmSegment> {
        let i = self.index_of_id(shmid)?;
        Some(&mut self.slots[i].as_mut().unwrap().segment)
    }

    fn remove(&mut self, shmid: i32) {
        if let Some(i) = self.index_of_id(shmid) {
            self.slots[i] = None;
        }
    }

    fn insert(&mut self, shmid: i32, segment: SysvShmSegment) -> Result<(), Errno> {
        let free = self
            .slots
            .iter()
            .position(|slot| slot.is_none())
            .ok_or(Errno::ENOSPC)?;
        self.slots[free] = Some(ShmSlot { shmid, segment });
        Ok(())
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// `shmget(key, size, shmflg)`.
    ///
    /// System V shared memory was entirely unimplemented, which is what stopped the webtop's
    /// video: X11's MIT-SHM extension is how a screen-capture client moves framebuffer bytes, and
    /// selkies' `pixelflux` capture aborts with a bare "shmget failed" without it -- the browser
    /// then sits on "Waiting for stream..." forever with no other diagnostic.
    pub(crate) fn sys_shmget(&self, key: i32, size: usize, shmflg: i32) -> Result<usize, Errno> {
        const IPC_PRIVATE: i32 = 0;
        const IPC_CREAT: i32 = 0o1000;
        const IPC_EXCL: i32 = 0o2000;

        let mut table = self.global.sysv_shm.lock();

        if key != IPC_PRIVATE
            && let Some(existing_idx) = table.index_of_key(key)
        {
            if shmflg & (IPC_CREAT | IPC_EXCL) == (IPC_CREAT | IPC_EXCL) {
                return Err(Errno::EEXIST);
            }
            // A caller asking for MORE than the existing segment holds cannot be satisfied by
            // handing it back, and silently returning a too-small segment would corrupt whatever
            // wrote past the end.
            let existing_slot = table.slots[existing_idx]
                .as_ref()
                .expect("index_of_key only ever returns an occupied slot");
            if size > existing_slot.segment.size {
                return Err(Errno::EINVAL);
            }
            return Ok(usize::try_from(existing_slot.shmid).unwrap());
        }

        if key != IPC_PRIVATE && shmflg & IPC_CREAT == 0 {
            return Err(Errno::ENOENT);
        }
        if size == 0 {
            return Err(Errno::EINVAL);
        }

        let page = litebox::mm::linux::PAGE_SIZE;
        let rounded = size.checked_next_multiple_of(page).ok_or(Errno::EINVAL)?;

        // No memory is actually created here -- matching real Linux, where `shmget` only
        // reserves an id/size and the first REAL mapping happens at `shmat` time, in whichever
        // process calls it (see `SysvShmSegment`'s own doc comment for why this changed: the
        // previous single-canonical-address design was wrong under cross-process fork).
        let shmid = self
            .global
            .next_shmid
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        table
            .insert(
                shmid,
                SysvShmSegment {
                    size: rounded,
                    key,
                    attaches: 0,
                    removed: false,
                },
            )
            .map_err(|_| Errno::ENOMEM)?;
        litebox_util_log::debug!(
            key:% = key, shmid:% = shmid, size:% = rounded;
            "sysv shm: created segment"
        );
        Ok(usize::try_from(shmid).unwrap())
    }

    /// `shmat(shmid, shmaddr, shmflg)`.
    ///
    /// Establishes a REAL mapping of the segment in the CALLING process's own address space --
    /// every process does this independently (including the creator's own first attach), via a
    /// named platform shared-memory object keyed by `shmid` (see `SysvShmSegment`'s own doc
    /// comment for why: a bare address handed to a non-creating process, the previous design,
    /// is wild/unmapped there under cross-process fork). The returned address is therefore
    /// per-process, matching real Linux (`shmat` gives no cross-process address guarantee
    /// either). A non-null `shmaddr` asking to place the segment somewhere specific is refused
    /// with `EINVAL` rather than honoured -- no caller in practice needs it (every real caller,
    /// including Xlib's MIT-SHM, passes `NULL`).
    pub(crate) fn sys_shmat(
        &self,
        shmid: i32,
        shmaddr: usize,
        _shmflg: i32,
    ) -> Result<usize, Errno> {
        if shmaddr != 0 {
            log_unsupported!("shmat with a caller-chosen address ({shmaddr:#x})");
            return Err(Errno::EINVAL);
        }
        let size = {
            let mut table = self.global.sysv_shm.lock();
            let Some(seg) = table.get_mut(shmid) else {
                return Err(Errno::EINVAL);
            };
            seg.attaches += 1;
            seg.size
        };
        let rollback_attach = || {
            let mut table = self.global.sysv_shm.lock();
            if let Some(seg) = table.get_mut(shmid) {
                seg.attaches = seg.attaches.saturating_sub(1);
            }
        };
        // Idempotent create-or-open by name (see `create_named_shared_memory`'s own doc comment):
        // the FIRST attacher (almost always the creator's own first `shmat`, since `shmget`
        // itself no longer maps anything -- see `SysvShmSegment`'s doc comment) creates the real
        // object; every later attacher, in any process, opens the SAME one by shmid.
        let name = alloc::format!("Local\\litebox_sysvshm_{shmid}");
        let handle = match self.global.platform.create_named_shared_memory(&name, size) {
            Ok(h) => h,
            Err(_) => {
                rollback_attach();
                return Err(Errno::ENOMEM);
            }
        };
        let Some(len) = litebox::mm::linux::NonZeroPageSize::new(size) else {
            rollback_attach();
            return Err(Errno::EINVAL);
        };
        // SAFETY: `handle` is a real shared-memory object sized to match `len`; mapping it at a
        // platform-chosen (non-fixed) address is sound -- no guest code has observed this address
        // range before this call returns it.
        let ptr = match unsafe {
            self.process().pm().map_existing_shared_pages(
                None,
                len,
                litebox::mm::linux::CreatePagesFlags::empty(),
                handle,
            )
        } {
            Ok(p) => p,
            Err(_) => {
                rollback_attach();
                return Err(Errno::ENOMEM);
            }
        };
        let addr = ptr.as_usize();
        self.files.borrow().record_shm_attachment(addr, shmid);
        litebox_util_log::debug!(
            shmid:% = shmid, addr:% = addr, size:% = size;
            "sysv shm: attached segment"
        );
        Ok(addr)
    }

    /// `shmdt(shmaddr)`.
    ///
    /// Matches the pre-51st-pass implementation's own scope: releases this process's
    /// bookkeeping (the shared attach count, and now the per-process reverse-lookup entry --
    /// see `FilesState::shm_attachments`'s doc comment) but does not actually `munmap` the local
    /// mapping. That was already true before this pass (the previous implementation never called
    /// `sys_munmap` either) and remains a real, pre-existing, documented gap -- not one this
    /// pass's fix introduces or widens -- because MIT-SHM/`-shmem` clients in practice keep a
    /// segment attached for the whole connection lifetime, never calling `shmdt` at all, so it is
    /// not on any path this investigation's own boot needs.
    pub(crate) fn sys_shmdt(&self, shmaddr: usize) -> Result<usize, Errno> {
        let Some(shmid) = self.files.borrow().take_shm_attachment(shmaddr) else {
            return Err(Errno::EINVAL);
        };
        let mut table = self.global.sysv_shm.lock();
        let drop_now = if let Some(seg) = table.get_mut(shmid) {
            seg.attaches = seg.attaches.saturating_sub(1);
            seg.removed && seg.attaches == 0
        } else {
            false
        };
        if drop_now {
            table.remove(shmid);
        }
        Ok(0)
    }

    /// `shmctl(shmid, cmd, buf)`.
    ///
    /// `IPC_RMID` and `IPC_STAT` are implemented; both are what MIT-SHM clients use (they attach,
    /// immediately mark the segment removed so it cannot leak, and keep using it until detach).
    pub(crate) fn sys_shmctl(
        &self,
        shmid: i32,
        cmd: i32,
        buf: Option<UserPtrMut<u8>>,
    ) -> Result<usize, Errno> {
        const IPC_RMID: i32 = 0;
        const IPC_STAT: i32 = 2;

        let mut table = self.global.sysv_shm.lock();
        let Some(seg) = table.get_mut(shmid) else {
            return Err(Errno::EINVAL);
        };

        match cmd {
            IPC_RMID => {
                seg.removed = true;
                let drop_now = seg.attaches == 0;
                if drop_now {
                    table.remove(shmid);
                }
                Ok(0)
            }
            IPC_STAT => {
                // `struct shmid_ds` on x86-64: a 48-byte `ipc_perm` followed by `shm_segsz`.
                // Only the size is meaningfully knowable here; the rest is zeroed rather than
                // fabricated.
                let Some(buf) = buf else {
                    return Err(Errno::EFAULT);
                };
                let size = seg.size;
                for i in 0..48isize {
                    let _ = buf.write_at_offset::<Platform>(i, 0u8);
                }
                for (i, b) in size.to_le_bytes().iter().enumerate() {
                    let off = 48isize + isize::try_from(i).unwrap();
                    let _ = buf.write_at_offset::<Platform>(off, *b);
                }
                Ok(0)
            }
            other => {
                log_unsupported!("shmctl cmd={other}");
                Err(Errno::EINVAL)
            }
        }
    }
}

/// Per-memfd real shared-memory state, keyed by the backing in-mem file's own `(dev, ino)` (see
/// `GlobalState::memfds`'s doc comment for why this lives shim-wide, mirroring
/// `syscalls::file::FlockRegistry`'s identical `(dev, ino)`-keying rationale).
pub(crate) struct MemfdEntry<Platform: PageManagementProvider<{ litebox::mm::linux::PAGE_SIZE }>> {
    pub(crate) handle: Platform::SharedMemoryHandle,
    /// The size `ftruncate` last set this memfd to (NOT necessarily page-aligned; `mmap`
    /// resolves against `size.next_multiple_of(PAGE_SIZE)`, matching `create_shared_memory`'s own
    /// page-rounding).
    pub(crate) size: usize,
    /// Whether `handle` has already been `mmap`'d by anyone since it was (re)created. The
    /// backing in-mem file's `Vec<u8>` is only ever written by `write()`/`pwrite()`, never by a
    /// peer's `mmap`'d writes -- so once a SECOND process (or the same process a second time,
    /// e.g. the compositor mapping a `wl_shm` pool the client already drew into through its own
    /// mapping) maps this handle, the `Vec<u8>` is stale and must NOT be re-copied over the
    /// shared object, or every write anyone has made through their own mapping is silently
    /// wiped back to whatever the guest last `write()`'d (usually zeros, since real Wayland/X11
    /// shm clients draw exclusively through their mapping and never call `write()` at all).
    /// Confirmed live: this is why every dumped frame ever captured under `--gui` showed only
    /// weston-desktop-shell's own repainted-every-second clock widget and nothing else -- every
    /// surface that painted once and then waited for damage got mmap-wiped back to black the
    /// moment the compositor mapped the client's pool.
    pub(crate) mapped: bool,
}
pub(crate) type MemfdRegistry<Platform> = BTreeMap<(usize, usize), MemfdEntry<Platform>>;

#[cfg(not(target_pointer_width = "64"))]
compile_error!("ELF patching code assumes 64-bit pointers (u64 <-> usize is lossless)");

#[cfg(target_arch = "x86_64")]
const ENDIAN: LittleEndian = LittleEndian;

/// Per-fd state for the shim's runtime ELF syscall rewriter.
///
/// Tracks base address and trampoline write cursor for each ELF file that
/// has executable segments mapped via `do_mmap_file()`.
pub(crate) struct ElfPatchState {
    /// Whether this file is already pre-patched (trampoline magic found at file tail).
    pre_patched: bool,
    /// For pre-patched binaries: file offset and size of the trampoline data.
    trampoline_file_offset: u64,
    trampoline_file_size: usize,
    /// Start address of the trampoline region (runtime).
    trampoline_addr: usize,
    /// Current write position within the trampoline (byte offset from `trampoline_addr`).
    trampoline_cursor: usize,
    /// Whether the trampoline region has been allocated.
    trampoline_mapped: bool,
    /// Total number of trampoline bytes currently mapped.
    trampoline_mapped_len: usize,
    /// Whether any runtime-generated stubs were successfully linked from code
    /// in this fd to the trampoline.
    runtime_patches_committed: bool,
    /// Tracks file-backed mappings for this fd as (vaddr, len) pairs.
    /// Used to find mappings that need patching when mprotect adds PROT_EXEC.
    /// Cleared on munmap to allow re-patching.
    file_mappings: BTreeSet<(usize, usize)>,
    /// Ranges that have already been patched by the runtime rewriter.
    /// This is a performance guard only — re-running the rewriter on
    /// already-patched code is safe because the second run will not see
    /// syscall instructions. Cleared on munmap alongside file_mappings.
    patched_ranges: BTreeSet<(usize, usize)>,
}

/// Key identifying one process's patching state for one of its file descriptors.
///
/// The `pid` component is load-bearing, not cosmetic. [`ElfPatchState`] holds *absolute*
/// guest addresses (`trampoline_addr`, `file_mappings`, `patched_ranges`) that are only
/// meaningful within the address space that produced them, while the cache itself lives on the
/// shim-wide `global` state that every `fork()`ed child shares by `Arc`. File descriptor numbers
/// are per-process and heavily reused (every process gets 0/1/2 and typically allocates 3, 4, ...
/// for the binaries it loads), so keying on `fd` alone made two unrelated processes collide on
/// one entry: one process could observe another's `patched_ranges` (skipping patching it still
/// needed), link stubs against another's `trampoline_addr`, or -- via
/// [`Task::finalize_elf_patch`] -- remove and `munmap` a trampoline region a concurrently
/// running sibling was still executing through.
///
/// The collision is not hypothetical: instrumenting `finalize_elf_patch` while running a
/// `fork()`/`execve`-heavy shell pipeline shows repeated calls for the same low fd numbers
/// (0, 1, 2, 3, 4, ...) from several different pids against this one shared map.
pub(crate) type ElfPatchKey = (i32, i32);

pub(crate) type ElfPatchCache = BTreeMap<ElfPatchKey, ElfPatchState>;

/// Identity of one code segment, as content rather than as a name.
///
/// `(device, inode)` identifies the FILE -- so the many hardlinked aliases of one library (mesa
/// ships fourteen DRI driver names for a single megadriver) share one entry, and a path that is
/// later replaced does not alias a stale scan -- and `(offset, len)` identifies the segment within
/// it. Deliberately NOT the path: two paths can be one file, and one path can become two files.
pub(crate) type SegmentScanKey = (u64, u64, usize, usize);

/// Executable code ranges per file; see [`crate::GlobalState::exec_ranges_cache`].
pub(crate) type ExecRangesCache =
    BTreeMap<(u64, u64), alloc::sync::Arc<alloc::vec::Vec<core::ops::Range<u64>>>>;

/// Scans shared by every mapping of a file; see [`crate::GlobalState::segment_scan_cache`].
pub(crate) type SegmentScanCache =
    BTreeMap<SegmentScanKey, alloc::sync::Arc<litebox_syscall_rewriter::SegmentScanTemplate>>;

#[inline]
fn align_up(addr: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (addr + align - 1) & !(align - 1)
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn align_down(addr: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    addr & !(align - 1)
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    #[inline]
    fn do_mmap(
        &self,
        suggested_addr: Option<usize>,
        len: usize,
        prot: ProtFlags,
        flags: MapFlags,
        ensure_space_after: bool,
        op: impl FnOnce(UserPtrMut<u8>) -> Result<usize, MappingError>,
    ) -> Result<UserPtrMut<u8>, MappingError> {
        litebox_common_linux::mm::do_mmap(
            &self.process().pm(),
            suggested_addr,
            len,
            prot,
            flags,
            ensure_space_after,
            op,
        )
    }

    #[inline]
    fn do_mmap_anonymous(
        &self,
        suggested_addr: Option<usize>,
        len: usize,
        prot: ProtFlags,
        flags: MapFlags,
    ) -> Result<UserPtrMut<u8>, MappingError> {
        let op = |_| Ok(0);
        self.do_mmap(suggested_addr, len, prot, flags, false, op)
    }

    fn do_mmap_file(
        &self,
        suggested_addr: Option<usize>,
        len: usize,
        prot: ProtFlags,
        flags: MapFlags,
        fd: i32,
        offset: usize,
    ) -> Result<UserPtrMut<u8>, MappingError> {
        let is_exec = prot.contains(ProtFlags::PROT_EXEC);

        // Perform the normal mmap first (CoW or memcpy fallback).
        let cow_attempt = cow_mmap_enabled()
            .then(|| self.try_cow_mmap_file(suggested_addr, len, &prot, &flags, fd, offset))
            .flatten();
        litebox_util_log::debug!(
            fd:% = fd, len:% = len, offset:% = offset,
            cow_took_path:% = cow_attempt.is_some();
            "DIAG do_mmap_file: path chosen"
        );
        let result = if let Some(cow_result) = cow_attempt {
            cow_result?
        } else {
            let memcpy_result = self.do_mmap_file_memcpy(suggested_addr, len, prot, flags, fd, offset);
            litebox_util_log::debug!(
                fd:% = fd, len:% = len, offset:% = offset,
                memcpy_ok:% = memcpy_result.is_ok(),
                memcpy_addr:% = memcpy_result.as_ref().map(|p| p.as_usize()).unwrap_or(0);
                "DIAG do_mmap_file: memcpy fallback result"
            );
            memcpy_result?
        };

        // AGENTS.md pass 260: log path<->address for every executable file-backed mapping, so a
        // future guest-exception capture's `rip` can be matched by hand against these ranges to
        // identify which shared library/binary actually crashed (litebox has no `/proc/self/maps`
        // for the guest to introspect itself -- this is the only available source of that
        // correlation, reusing the same `lookup_fd_path` mechanism `readlink("/proc/self/fd/N")`
        // already relies on).
        if is_exec {
            let path = self.files.borrow().lookup_fd_path(fd as usize);
            litebox_util_log::debug!(
                path:? = path, start:% = result.as_usize(), len:% = len, offset:% = offset;
                "diag-exec-mmap: tracking for future crash-address correlation"
            );
        }

        // Runtime syscall rewriting: patch PROT_EXEC segments in-place.
        if is_exec {
            let syscall_entry = self.global.platform.get_syscall_entry_point();
            if syscall_entry != 0
                && !self.maybe_patch_exec_segment(result, len, fd, syscall_entry, Some(offset))
            {
                // Trampoline setup failed for a pre-patched binary whose
                // .text already contains JMPs to the trampoline address.
                // Continuing would guarantee a SIGSEGV on the first
                // rewritten syscall, so fail the mmap instead.
                let _ = self.sys_munmap(result, len);
                return Err(MappingError::OutOfMemory);
            }
        } else {
            // Ensure patch state is initialized for this fd (no-op if already done).
            self.init_elf_patch_state(fd, result.as_usize(), offset);
            // Track non-exec file mappings so we can patch them if they later
            // gain PROT_EXEC via mprotect.
            let mut cache = self.global.elf_patch_cache.lock();
            if let Some(state) = cache.get_mut(&self.elf_patch_key(fd)) {
                let mapping_key = (result.as_usize(), len);
                // Overlapping entries are safe here: file_mappings is only used
                // to know which (addr, len) ranges belong to this fd so we can
                // patch them later if mprotect adds PROT_EXEC.  Duplicates or
                // overlaps are harmless — the patching logic is idempotent.
                state.file_mappings.insert(mapping_key);
            }
        }

        Ok(result)
    }

    /// Attempt to create a CoW mapping for a file with static backing data.
    ///
    /// Returns `Some(result)` if CoW was attempted (success or failure),
    /// `None` if CoW is not applicable (fall back to memcpy).
    // TODO(jb): does this need to be Option-Result or can it just be Option?
    fn try_cow_mmap_file(
        &self,
        suggested_addr: Option<usize>,
        len: usize,
        prot: &ProtFlags,
        flags: &MapFlags,
        fd: i32,
        offset: usize,
    ) -> Option<Result<UserPtrMut<u8>, MappingError>> {
        if !len.is_multiple_of(PAGE_SIZE) {
            return None;
        }

        let Ok(fd) = u32::try_from(fd).and_then(usize::try_from) else {
            return None;
        };

        let files = self.files.borrow();
        let raw_fd = fd;

        let static_data = files
            .run_on_raw_fd(
                raw_fd,
                |typed_fd| files.fs.get_static_backing_data(typed_fd),
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None)
            .ok()??;
        // DIAG (AGENTS.md pass 223/224): confirm empirically whether this CoW mapping path is
        // even reached for the calls that end up EEXIST-failing, since pass 223's own code
        // reading could not fully rule it out without a live capture. `try_allocate_cow_pages`
        // IS implemented on Windows userland (`WindowsUserland::try_allocate_cow_pages`,
        // `litebox_platform_windows_userland/src/lib.rs`: `CreateFileMappingW` +
        // `MapViewOfFile3`) -- this print exists to check whether reaching this far (i.e.
        // `get_static_backing_data` succeeding) at all correlates with the still-open EEXIST
        // regression.
        litebox_util_log::debug!(
            tid:% = self.tid.get(), offset:% = offset, static_len:% = static_data.len();
            "DIAG try_cow_mmap_file: static_data resolved, about to attempt CoW"
        );

        if offset > static_data.len() {
            return None;
        }

        let available_len = static_data.len().saturating_sub(offset);
        if available_len < len {
            // Cannot fill full page
            return None;
        }

        let fixed_behavior = if flags.contains(MapFlags::MAP_FIXED_NOREPLACE) {
            FixedAddressBehavior::NoReplace
        } else if flags.contains(MapFlags::MAP_FIXED) {
            FixedAddressBehavior::Replace
        } else {
            FixedAddressBehavior::Hint
        };

        let permissions = {
            let mut perms = MemoryRegionPermissions::empty();
            perms.set(
                MemoryRegionPermissions::READ,
                prot.contains(ProtFlags::PROT_READ),
            );
            perms.set(
                MemoryRegionPermissions::WRITE,
                prot.contains(ProtFlags::PROT_WRITE),
            );
            perms.set(
                MemoryRegionPermissions::EXEC,
                prot.contains(ProtFlags::PROT_EXEC),
            );
            perms
        };

        // How much padding space, immediately BEFORE `suggested_addr`, this call is willing to
        // let a platform's `try_allocate_cow_pages` use for a misaligned-file-offset workaround
        // (see that trait method's own doc comment in `litebox/src/platform/page_mgmt.rs` for
        // the full contract, and `docs/cow-mmap-fixed-address-design.md` for the design this
        // implements). Bounded at `MAX_COW_VERIFIED_PADDING` (60KiB): the largest padding ANY
        // known platform constraint (Windows' 64KiB `MapViewOfFile3` allocation granularity
        // minus one page) could ever need -- a generous upper bound to QUERY, not a promise that
        // this much is actually free; the live query below determines the real, safe amount.
        //
        // THIS QUERY IS THE ENTIRE SAFETY BOUNDARY for the padding trick: it asks `Vmem`'s own,
        // CURRENT, live state (not a static inference about what the ELF loader's reservation
        // "should" contain) whether the exact byte range immediately preceding `suggested_addr`
        // is a single, contiguous, `PROT_NONE` (fully inaccessible) mapping -- i.e. genuinely
        // still part of this process's own untouched ELF reservation slack, never anything a
        // platform implementation infers or assumes on its own (see that trait method's doc
        // comment for why: it structurally has no `Vmem` access to check this itself). Any
        // answer other than "yes, PROT_NONE, for the full requested window" makes
        // `verified_safe_padding` clamp down to exactly how much (if any) genuinely qualifies --
        // `try_allocate_cow_pages` NEVER receives a padding budget this call has not itself,
        // just now, confirmed live.
        const MAX_COW_VERIFIED_PADDING: usize = 0x1_0000 - PAGE_SIZE;
        let verified_safe_padding = suggested_addr
            .filter(|&addr| addr >= MAX_COW_VERIFIED_PADDING)
            .and_then(|addr| {
                // Query progressively smaller windows (in page steps) rather than only the
                // maximal one: `get_memory_permissions` returns `None` for ANY partial overlap
                // (see its own doc comment / `litebox/src/mm/linux.rs`), so a reservation that
                // genuinely has, say, 8KiB of real PROT_NONE slack immediately before
                // `suggested_addr` (not the full 60KiB max) would otherwise report "unsafe" for
                // the whole window and get zero padding credit, even though a smaller amount is
                // fully safe and would still unlock the common case. Try from the largest window
                // down to one page, first `Some` hit wins.
                (1..=MAX_COW_VERIFIED_PADDING / PAGE_SIZE)
                    .rev()
                    .map(|n| n * PAGE_SIZE)
                    .find_map(|candidate| {
                        let start = addr.checked_sub(candidate)?;
                        let ptr = litebox::mm::linux::NonZeroAddress::<PAGE_SIZE>::new(start)?;
                        let size = litebox::mm::linux::NonZeroPageSize::<PAGE_SIZE>::new(
                            candidate,
                        )?;
                        let perms = self.process().pm().get_memory_permissions(ptr, size)?;
                        // `PROT_NONE` == no permission bits set at all -- anything else (even a
                        // READ-only mapping) is real content this call must not overwrite.
                        perms.is_empty().then_some(candidate)
                    })
            })
            .unwrap_or(0);

        // XXX: `try_allocate_cow_pages` and `register_existing_mapping` are not called under a
        // unified lock, so there is a theoretical race if two threads concurrently attempt a
        // fixed-address mapping with replacement at the same address. In practice this is benign:
        // if a program races like this both threads will register the same mapping anyway. Updating
        // to a begin/attempt/commit scheme could close this race window entirely.
        match <_ as PageManagementProvider<{ PAGE_SIZE }>>::try_allocate_cow_pages(
            self.global.platform,
            suggested_addr.unwrap_or(0),
            &static_data[offset..offset + len],
            permissions,
            fixed_behavior,
            verified_safe_padding,
        ) {
            Ok((ptr, padding_range)) => {
                // AGENTS.md pass 212: this CoW mapping path bypasses `litebox_common_linux::mm
                // ::do_mmap`'s own shared fixed-address-mismatch check (this crate's other
                // mmap path, `do_mmap_file_memcpy`, goes through it) by calling
                // `try_allocate_cow_pages` directly -- needs the same guard. Real Linux's
                // `MAP_FIXED` contract is "map exactly here or fail", never silently relocate;
                // a platform's own `allocate_pages` can still choose to relocate a `Replace`
                // request away from a foreign-claimed range rather than corrupt another live
                // process (see `litebox_platform_windows_userland`'s `allocate_pages`), and this
                // was root-caused (pass 212) to letting the ELF loader's BSS zero-fill target
                // completely unmapped memory when that relocation silently happened underneath
                // a `MAP_FIXED` ELF-segment mapping.
                if fixed_behavior == FixedAddressBehavior::Replace
                    && let Some(requested) = suggested_addr
                    && ptr.as_usize() != requested
                {
                    return Some(Err(MappingError::OutOfMemory));
                }
                // Register any padding prefix the platform ALSO host-mapped BEFORE registering
                // (or letting the guest observe) the real content range -- this ordering is the
                // whole point of the caller-verifies/platform-executes split (see the trait
                // method's own doc comment): there must never be a window where `Vmem` doesn't
                // yet know about host-mapped memory. `replace: true` mirrors the content
                // registration below (a padding range can, in principle, coincide with a range
                // this same reservation already holds -- an ordinary PROT_NONE-over-PROT_NONE
                // overwrite is a correct no-op, never a real conflict, since this is
                // this-process-owned slack by construction of the live query above, never
                // another mapping's space).
                if let Some((padding_start, padding_len)) = padding_range {
                    let padding_range = PageRange::new(padding_start, padding_start + padding_len)
                        .expect("platform-reported padding range must be page-aligned");
                    // SAFETY: `padding_start..padding_start+padding_len` is exactly the host-
                    // mapped-but-guest-inaccessible range `try_allocate_cow_pages` just created
                    // (per its own contract) as part of the SAME view as the content range below
                    // -- registering it here, before this function returns and before the guest
                    // can resume, closes the pass-343/344 untracked-memory window by
                    // construction.
                    unsafe {
                        self.process().pm().register_existing_mapping(
                            padding_range,
                            MemoryRegionPermissions::empty(),
                            true,
                            true,
                            flags.contains(MapFlags::MAP_SHARED),
                        )
                    }
                    .unwrap();
                }
                let range =
                    PageRange::new(ptr.as_usize(), ptr.as_usize().checked_add(len).unwrap())
                        .unwrap();
                // SAFETY: ptr is the freshly CoW-mapped region of exactly `len` bytes with
                // `permissions`.
                unsafe {
                    self.process().pm().register_existing_mapping(
                        range,
                        permissions,
                        true,
                        fixed_behavior == FixedAddressBehavior::Replace,
                        flags.contains(MapFlags::MAP_SHARED),
                    )
                }
                .unwrap();
                Some(Ok(UserPtrMut::from_platform_ptr::<Platform>(ptr)))
            }
            Err(_cow_not_supported) => None,
        }
    }

    /// Fallback mmap implementation using page-by-page memcpy, for files where the CoW attempt
    /// fails (either due to lack of support on platform, or non-static-backed data, etc.)
    fn do_mmap_file_memcpy(
        &self,
        suggested_addr: Option<usize>,
        len: usize,
        prot: ProtFlags,
        flags: MapFlags,
        fd: i32,
        offset: usize,
    ) -> Result<UserPtrMut<u8>, MappingError> {
        let op = |ptr: UserPtrMut<u8>| -> Result<usize, MappingError> {
            // Note a malicious user may unmap ptr while we are reading.
            // `sys_read` does not handle page faults, so we need to use a
            // temporary buffer to read the data from fs (without worrying page
            // faults) and write it to the user buffer with page fault handling.
            let mut file_offset = offset;
            let mut buffer = [0; PAGE_SIZE];
            let mut copied = 0;
            while copied < len {
                let size = match self.sys_read(fd, &mut buffer, Some(file_offset)) {
                    Ok(size) => size,
                    // Real Linux's mmap() is not among the syscalls interruptible by a signal
                    // (see signal(7)): a signal arriving while the kernel is populating a
                    // freshly mmap'd region never causes mmap() itself to return EINTR to the
                    // caller. This fallback path does its file-reading via an internal
                    // `sys_read()` call that *can* surface EINTR (e.g. a timer signal landing
                    // mid-copy), but that's a shim implementation detail of *this* fallback,
                    // not something a real mmap() caller would ever observe -- so retry instead
                    // of propagating it as a (bogus) mmap() failure.
                    Err(Errno::EINTR) => continue,
                    Err(Errno::EBADF) => return Err(MappingError::BadFD(fd)),
                    Err(Errno::EISDIR) => return Err(MappingError::NotAFile),
                    Err(Errno::EACCES) => return Err(MappingError::NotForReading),
                    // Any other, genuinely unexpected read failure (e.g. EIO from the backing
                    // filesystem) used to panic here; report it as a mapping I/O error instead.
                    Err(_) => return Err(MappingError::Io),
                };
                if size == 0 {
                    break;
                }
                // ptr is a valid pointer returned by do_mmap.
                ptr.copy_from_slice::<Platform>(copied, &buffer[..size])
                    .unwrap();
                copied += size;
                file_offset += size;
            }
            litebox_util_log::debug!(
                fd:% = fd, requested_len:% = len, copied:% = copied, offset:% = offset;
                "DIAG do_mmap_file_memcpy: copy loop finished"
            );
            Ok(copied)
        };
        let fixed_addr = flags.intersects(MapFlags::MAP_FIXED | MapFlags::MAP_FIXED_NOREPLACE);
        self.do_mmap(
            suggested_addr,
            len,
            prot,
            flags,
            // Note we need to ensure that the space after the mapping is available
            // so that we could load trampoline code right after the mapping.
            offset == 0 && !fixed_addr,
            op,
        )
    }

    /// If `fd` is a `memfd_create`-backed fd, map the guest's requested range directly onto its
    /// real (host-backed) shared-memory storage and return `Some(result)` -- mirrors
    /// `try_dri_dumb_buffer_mmap` immediately below exactly, the same "an ordinary fs-kind fd
    /// carries a real shared-memory handle on the side, keyed by `(dev, ino)` in
    /// `GlobalState::memfds`" shape DRM already established, since a plain in-mem file's own
    /// bytes (an ordinary `Vec<u8>`, see `in_mem::FileSystem::truncate`) are guest heap memory,
    /// not a real OS-level shared-memory object `MAP_SHARED|PROT_WRITE` could safely alias.
    /// Returns `None` for any fd that isn't a live memfd, so the caller falls through to the
    /// ordinary file-backed-mapping path (which correctly rejects `MAP_SHARED|PROT_WRITE` on a
    /// real file, since this shim has no write-back-to-file story for that case).
    fn try_memfd_mmap(
        &self,
        addr: usize,
        len: usize,
        flags: &MapFlags,
        fd: i32,
        offset: usize,
    ) -> Option<Result<UserPtrMut<u8>, MappingError>> {
        let raw_fd = u32::try_from(fd).ok().map(|v| v as usize)?;
        let files = self.files.borrow();
        // Captures both the memfd identity key AND (if this fd is one) the file's CURRENT bytes
        // in one lookup, so the sync step below never needs a second, separate fd resolution.
        let (key, current_bytes) = files
            .run_on_raw_fd(
                raw_fd,
                |typed_fd| {
                    let status = files.fs.fd_file_status(typed_fd).ok()?;
                    let key = (status.node_info.dev, status.node_info.ino);
                    let mut buf = alloc::vec![0u8; status.size];
                    let n = files.fs.read(typed_fd, &mut buf, Some(0)).unwrap_or(0);
                    buf.truncate(n);
                    Some((key, buf))
                },
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None)
            .ok()
            .flatten()?;
        let mut memfds = self.global.memfds.lock();
        let entry = memfds.get_mut(&key)?;
        // A memfd's backing shared-memory object is exactly `entry.size` bytes (the last
        // `ftruncate`'d size, rounded up to a whole page by `create_shared_memory` itself); a
        // client mapping a stale offset/length past that (e.g. before ever calling `ftruncate`,
        // or after shrinking it) gets a real `SIGBUS`-territory rejection, matching real Linux.
        let aligned_len = align_up(len, PAGE_SIZE);
        if offset != 0 || aligned_len > entry.size.next_multiple_of(PAGE_SIZE) {
            return Some(Err(MappingError::UnAligned));
        }
        let handle = entry.handle;
        // See `MemfdEntry::mapped`'s own doc comment: only the FIRST `mmap` of a given handle
        // may sync the in-mem `Vec<u8>` into the shared object -- every mmap after that must
        // leave the shared object's own live contents alone, or a second mapper (typically the
        // compositor, mapping a `wl_shm` pool the client already drew into through its own
        // mapping) wipes everything the first mapper wrote.
        let already_mapped = entry.mapped;
        entry.mapped = true;
        drop(memfds);
        drop(files);
        // Sync in whatever bytes the guest already wrote via ordinary `write()`/`pwrite()` calls
        // before ever mmapping (the real Wayland `wl_shm` pattern this bridges: `ftruncate` then
        // `write()` the pixel data, THEN the peer -- typically a different process/thread, e.g.
        // the compositor -- `mmap()`s the same fd to read it, see this function's own doc comment
        // for why an ordinary in-mem file can't support `MAP_SHARED|PROT_WRITE` directly). A
        // transient, private, exclusively-owned mapping the caller never observes -- copies bytes
        // in and unmaps immediately, before returning the REAL mapping requested below. Skipped
        // entirely once `already_mapped`, since the shared object is now the sole source of
        // truth and re-syncing from the (now-stale) `Vec<u8>` would destroy live content.
        if !already_mapped
            && let Some(sync_len) = litebox::mm::linux::NonZeroPageSize::new(aligned_len)
        {
            // SAFETY: a fresh, private, non-fixed mapping of `handle` -- no guest code has ever
            // observed this address, so writing into it and unmapping it immediately after is
            // sound; `handle` itself outlives this transient mapping (owned by `memfds`).
            if let Ok(ptr) = unsafe {
                self.process().pm().map_existing_shared_pages(
                    None,
                    sync_len,
                    litebox::mm::linux::CreatePagesFlags::empty(),
                    handle,
                )
            } {
                let copy_len = current_bytes.len().min(aligned_len);
                let _ = ptr.write_slice_at_offset(0, &current_bytes[..copy_len]);
                let user_ptr = UserPtrMut::from_platform_ptr::<Platform>(ptr);
                let _ =
                    litebox_common_linux::mm::sys_munmap(&self.process().pm(), user_ptr, aligned_len);
            }
        }
        let suggested_addr = if addr == 0 { None } else { Some(addr) };
        let create_flags = {
            let mut f = litebox::mm::linux::CreatePagesFlags::empty();
            f.set(
                litebox::mm::linux::CreatePagesFlags::FIXED_ADDR,
                flags.intersects(MapFlags::MAP_FIXED | MapFlags::MAP_FIXED_NOREPLACE),
            );
            f.set(
                litebox::mm::linux::CreatePagesFlags::NOREPLACE,
                flags.contains(MapFlags::MAP_FIXED_NOREPLACE),
            );
            f
        };
        let suggested_addr = match suggested_addr {
            Some(a) => match litebox::mm::linux::NonZeroAddress::new(a) {
                Some(n) => Some(n),
                None => return Some(Err(MappingError::UnAligned)),
            },
            None => None,
        };
        let Some(length) = litebox::mm::linux::NonZeroPageSize::new(aligned_len) else {
            return Some(Err(MappingError::UnAligned));
        };
        // Note: `map_shared_memory` (litebox_platform_windows_userland/src/lib.rs) already logs
        // a `nonzero_in_sample` content digest for every mapping it establishes, gated behind
        // `LITEBOX_DRM_TRACE=1` -- this is the "sample the CLIENT buffer like the scanout buffer"
        // instrumentation the investigation needs (see AGENTS.md's "Rendering/scanout blocker"
        // section); no separate digest is needed here.
        Some(
            unsafe {
                self.process()
                    .pm()
                    .map_existing_shared_pages(suggested_addr, length, create_flags, handle)
            }
            .map(UserPtrMut::from_platform_ptr::<Platform>),
        )
    }

    /// If `fd` is an ordinary file and the guest asked for a `MAP_SHARED` mapping, back it with a
    /// real shared-memory object -- keyed by the file's `(dev, ino)`, so every process mapping the
    /// same file binds to the SAME object and sees the others' writes -- and return `Some(result)`.
    /// Returns `None` for anonymous or `MAP_PRIVATE` mappings, which keep their existing paths.
    ///
    /// Read-only mappers must go through here too, not just writable ones. `MAP_SHARED` means
    /// "these mappers see each other's writes", and that is precisely a property that cannot be
    /// delivered by giving the reader its own snapshot of the file's bytes while the writer gets
    /// a shared object -- they would simply be different memory. dconf is built out of exactly
    /// that asymmetry: the writer (`dconf_shm_flag`) maps the flag byte `PROT_WRITE`, while every
    /// reader (`dconf_shm_open`) maps the same byte `PROT_READ`, and the reader polls it to learn
    /// that its cached copy of the database is stale.
    ///
    /// This exists because rejecting the combination outright with `ENODEV` (as the check just
    /// below this call site used to do for every file) is not a survivable answer for the callers
    /// that use it. `dconf` -- and therefore every GSettings write in a MATE, GNOME or XFCE
    /// session -- does exactly this, in `shm/dconf-shm.c`:
    ///
    /// ```text
    ///     fd  = open (".../dconf/user", O_RDWR | O_CREAT, 0600);
    ///     ftruncate (fd, 1);
    ///     shm = mmap (NULL, 1, PROT_WRITE, MAP_SHARED, fd, 0);
    ///     close (fd);
    ///     g_assert (shm != MAP_FAILED);
    /// ```
    ///
    /// so `ENODEV` there is not graceful degradation, it is
    /// `dconf:ERROR:../shm/dconf-shm.c:142:dconf_shm_flag: assertion failed: (shm != MAP_FAILED)`
    /// and `dconf-service` aborting mid-call. Every dconf write afterwards then fails with
    /// `GDBus.Error:...NoReply: Message recipient disconnected from message bus without replying`
    /// -- which is precisely why `mate-panel` came up with no panels at all under LiteBox: a
    /// panel's entire layout (`org.mate.panel`'s toplevel list) lives in dconf, and an empty
    /// toplevel list means zero panels, with no error of its own to show for it.
    ///
    /// Note the `PROT_WRITE` with no `PROT_READ` above: that is legal on Linux and is what dconf
    /// asks for, so this path must not assume a readable mapping. It does not -- the platform
    /// layer already widens write-only to read/write, since Windows has no write-only page
    /// protection.
    ///
    /// KNOWN LIMITATIONS, stated rather than papered over. Both are consequences of the shared
    /// object being a separate allocation from the file's own byte storage:
    ///
    /// 1. Writes through the mapping are visible to every other MAPPER of the file, but are not
    ///    propagated back into its byte storage, so a later `read()` still returns the pre-`mmap`
    ///    contents. Closing that needs a write-back path this shim has nowhere to hang: the
    ///    mapping outlives the descriptor (POSIX requires that, and the dconf sequence above
    ///    closes the fd immediately after mapping), and there is no fd-to-path or open-by-inode
    ///    route to reacquire the file at `munmap`/`msync` time.
    /// 2. The object is seeded from the file once, by the first mapper. A file rewritten IN PLACE
    ///    with `write()` afterwards will not show its new bytes to mappers. Rewriting by
    ///    `rename()` over the top -- what dconf-service itself does with the database, and the
    ///    normal atomic-replace idiom -- is unaffected, because the replacement is a different
    ///    inode and therefore a different key, hence a fresh object seeded from the new contents.
    fn try_shared_file_mmap(
        &self,
        addr: usize,
        len: usize,
        flags: &MapFlags,
        fd: i32,
        offset: usize,
    ) -> Option<Result<UserPtrMut<u8>, MappingError>> {
        if flags.contains(MapFlags::MAP_ANONYMOUS) || !flags.contains(MapFlags::MAP_SHARED) {
            return None;
        }
        // Only whole-file mappings from offset 0 share an object here. A non-zero offset would
        // need per-offset objects to stay coherent with each other, and nothing observed asks
        // for one -- falling through leaves such a call on the old `ENODEV` answer rather than
        // silently giving it an incoherent mapping.
        if offset != 0 {
            return None;
        }
        let raw_fd = u32::try_from(fd).ok().map(|v| v as usize)?;
        let files = self.files.borrow();
        // One lookup for both the identity key and the file's CURRENT bytes, exactly as
        // `try_memfd_mmap` does -- the seed step below must not need a second fd resolution.
        let (key, current_bytes) = files
            .run_on_raw_fd(
                raw_fd,
                |typed_fd| {
                    let status = files.fs.fd_file_status(typed_fd).ok()?;
                    let key = (status.node_info.dev, status.node_info.ino);
                    let mut buf = alloc::vec![0u8; status.size];
                    let n = files.fs.read(typed_fd, &mut buf, Some(0)).unwrap_or(0);
                    buf.truncate(n);
                    Some((key, buf))
                },
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None)
            .ok()
            .flatten()?;
        drop(files);

        let aligned_len = align_up(len, PAGE_SIZE);
        let Some(length) = litebox::mm::linux::NonZeroPageSize::new(aligned_len) else {
            return Some(Err(MappingError::UnAligned));
        };

        let mut shared = self.global.shared_files.lock();
        let (handle, already_mapped) = match shared.get_mut(&key) {
            // Reuse only when the existing object is big enough for what is being asked for.
            // Handing back a shorter one would let the guest address past its end.
            Some(entry) if entry.size >= aligned_len => {
                let was = entry.mapped;
                entry.mapped = true;
                (entry.handle, was)
            }
            _ => {
                let handle = self
                    .global
                    .platform
                    .create_shared_memory(aligned_len)
                    .ok()?;
                shared.insert(
                    key,
                    MemfdEntry {
                        handle,
                        size: aligned_len,
                        mapped: true,
                    },
                );
                (handle, false)
            }
        };
        drop(shared);

        // Seed the object from the file's current bytes on the FIRST mapping only. After that the
        // shared object is the sole source of truth, and re-copying the (now stale) file bytes
        // over it would wipe whatever other mappers have written -- the same hazard
        // `MemfdEntry::mapped` documents at length for memfds.
        if !already_mapped && !current_bytes.is_empty() {
            // SAFETY: a fresh, private, non-fixed mapping of `handle` -- no guest code has ever
            // observed this address, so writing into it and unmapping it immediately is sound;
            // `handle` itself outlives this transient mapping (owned by `shared_files`).
            if let Ok(ptr) = unsafe {
                self.process().pm().map_existing_shared_pages(
                    None,
                    length,
                    litebox::mm::linux::CreatePagesFlags::empty(),
                    handle,
                )
            } {
                let copy_len = current_bytes.len().min(aligned_len);
                let _ = ptr.write_slice_at_offset(0, &current_bytes[..copy_len]);
                let user_ptr = UserPtrMut::from_platform_ptr::<Platform>(ptr);
                let _ = litebox_common_linux::mm::sys_munmap(
                    &self.process().pm(),
                    user_ptr,
                    aligned_len,
                );
            }
        }

        let create_flags = {
            let mut f = litebox::mm::linux::CreatePagesFlags::empty();
            f.set(
                litebox::mm::linux::CreatePagesFlags::FIXED_ADDR,
                flags.intersects(MapFlags::MAP_FIXED | MapFlags::MAP_FIXED_NOREPLACE),
            );
            f.set(
                litebox::mm::linux::CreatePagesFlags::NOREPLACE,
                flags.contains(MapFlags::MAP_FIXED_NOREPLACE),
            );
            f
        };
        let suggested_addr = match if addr == 0 { None } else { Some(addr) } {
            Some(a) => match litebox::mm::linux::NonZeroAddress::new(a) {
                Some(n) => Some(n),
                None => return Some(Err(MappingError::UnAligned)),
            },
            None => None,
        };
        Some(
            unsafe {
                self.process()
                    .pm()
                    .map_existing_shared_pages(suggested_addr, length, create_flags, handle)
            }
            .map(UserPtrMut::from_platform_ptr::<Platform>),
        )
    }

    /// If `fd` is a DRM device fd and `offset` is a fake offset a prior `DRM_IOCTL_MODE_MAP_DUMB`
    /// call handed out, map the guest's requested range directly onto that dumb buffer's real
    /// (host-backed) storage and return `Some(result)`. Returns `None` for any other `fd` (not a
    /// DRI device, or a DRI device but `offset` doesn't match any known dumb buffer -- e.g. a
    /// client's own bug, or an offset from a buffer already destroyed), so the caller falls
    /// through to the ordinary file-backed-mapping path (which will itself reject it -- there is
    /// nothing else valid to mmap a DRM fd at).
    fn try_dri_dumb_buffer_mmap(
        &self,
        addr: usize,
        len: usize,
        prot: &ProtFlags,
        flags: &MapFlags,
        fd: i32,
        offset: usize,
    ) -> Option<Result<UserPtrMut<u8>, MappingError>> {
        let raw_fd = u32::try_from(fd).ok().map(|v| v as usize)?;
        let files = self.files.borrow();
        let is_dri = files
            .run_on_raw_fd(
                raw_fd,
                |typed_fd| self.is_dri_device(&files.fs, typed_fd).unwrap_or(false),
                |_| false,
                |_| false,
                |_| false,
                |_| false,
                |_| false,
                |_| false,
                |_| false,
                |_| false,
                |_| false,
            )
            .unwrap_or(false);
        if !is_dri {
            return None;
        }
        // A `DRM_IOCTL_PRIME_HANDLE_TO_FD`-exported fd (see `DrmPrimeFdMarker`'s own doc comment,
        // `syscalls::file`) carries the exported buffer's fake `MAP_DUMB` offset as PER-FD
        // metadata -- real PRIME/dma-buf fds are always mapped at offset 0 by the caller (there
        // is no second offset namespace the way the original DRM device fd's `MAP_DUMB` has one),
        // so this resolves the buffer from the fd's own tag rather than from the guest-supplied
        // `offset` argument, which is expected to be `0` here.
        let prime_map_offset = files
            .run_on_raw_fd(
                raw_fd,
                |typed_fd| {
                    self.global
                        .litebox
                        .descriptor_table()
                        .with_metadata(typed_fd, |m: &crate::syscalls::file::DrmPrimeFdMarker| {
                            m.map_offset
                        })
                        .ok()
                },
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
            )
            .ok()
            .flatten();
        let effective_offset = prime_map_offset.unwrap_or(offset as u64);
        let (shared_handle, buffer_size) = self.global.drm.lookup_by_map_offset(effective_offset)?;
        let aligned_len = align_up(len, PAGE_SIZE);
        if aligned_len > buffer_size.next_multiple_of(PAGE_SIZE) {
            // Guest asked to map more than the buffer actually holds -- real Linux rejects an
            // out-of-range dumb-buffer mmap the same way (`SIGBUS`-on-access territory otherwise).
            return Some(Err(MappingError::UnAligned));
        }
        let suggested_addr = if addr == 0 { None } else { Some(addr) };
        let create_flags = {
            let mut f = litebox::mm::linux::CreatePagesFlags::empty();
            f.set(
                litebox::mm::linux::CreatePagesFlags::FIXED_ADDR,
                flags.intersects(MapFlags::MAP_FIXED | MapFlags::MAP_FIXED_NOREPLACE),
            );
            f.set(
                litebox::mm::linux::CreatePagesFlags::NOREPLACE,
                flags.contains(MapFlags::MAP_FIXED_NOREPLACE),
            );
            f
        };
        let suggested_addr = match suggested_addr {
            Some(a) => match litebox::mm::linux::NonZeroAddress::new(a) {
                Some(n) => Some(n),
                None => return Some(Err(MappingError::UnAligned)),
            },
            None => None,
        };
        let Some(length) = litebox::mm::linux::NonZeroPageSize::new(aligned_len) else {
            return Some(Err(MappingError::UnAligned));
        };
        let _ = prot;
        let result = unsafe {
            self.process()
                .pm()
                .map_existing_shared_pages(suggested_addr, length, create_flags, shared_handle)
        };
        // Log the GUEST-visible address this dumb buffer lands at -- unlike the host
        // presentation thread's own transient per-flip mapping (a fresh address every
        // time), this is the fixed address weston's own process actually reads/writes
        // through for the buffer's whole lifetime, which is what a decommit/unmap
        // range needs to be compared against to catch a cross-process reclaim hitting
        // this VMA.
        if let Ok(ptr) = &result {
            litebox_util_log::debug!(
                addr:% = ptr.as_usize(), len:% = aligned_len;
                "diag-drm-fb-addr"
            );
        }
        Some(result.map(UserPtrMut::from_platform_ptr::<Platform>))
    }

    /// Handle syscall `mmap`
    pub(crate) fn sys_mmap(
        &self,
        addr: usize,
        len: usize,
        prot: ProtFlags,
        flags: MapFlags,
        fd: i32,
        offset: usize,
    ) -> Result<UserPtrMut<u8>, Errno> {
        // 2026-09-10 Track-B Xvfb-crash investigation (docs/track-b-fork-fix-progress.md):
        // "DIAG sys_mmap: entry" below only logs the REQUESTED parameters, never the actual
        // returned address for an `addr == 0` (let-the-platform-choose) call -- which is every
        // anonymous mmap a dynamic linker makes for a library's BSS/TLS-bookkeeping tail. This
        // wrapper logs the result too, closing that gap: lets a future capture directly answer
        // "did any mmap call's RETURNED range overlap the crash region" instead of only "was
        // that exact size ever requested".
        let result = self.sys_mmap_inner(addr, len, prot, flags, fd, offset);
        litebox_util_log::debug!(
            tid:% = self.tid.get(), addr:% = addr, len:% = len,
            ok:% = result.is_ok(),
            returned_start:% = result.as_ref().map(|p| p.as_usize()).unwrap_or(0),
            returned_end:% = result.as_ref().map(|p| p.as_usize() + len).unwrap_or(0);
            "DIAG sys_mmap: result"
        );
        result
    }

    fn sys_mmap_inner(
        &self,
        addr: usize,
        len: usize,
        prot: ProtFlags,
        flags: MapFlags,
        fd: i32,
        offset: usize,
    ) -> Result<UserPtrMut<u8>, Errno> {
        litebox_util_log::debug!(
            tid:% = self.tid.get(), addr:% = addr, len:% = len, prot:? = prot, flags:? = flags,
            fd:% = fd, offset:% = offset;
            "DIAG sys_mmap: entry"
        );
        // check alignment
        if !offset.is_multiple_of(PAGE_SIZE) || !addr.is_multiple_of(PAGE_SIZE) || len == 0 {
            return Err(Errno::EINVAL);
        }

        // A DRM dumb-buffer `mmap()` (real clients always use `MAP_SHARED | PROT_WRITE` here --
        // they need their pixel writes to reach the buffer the kernel/scanout also reads) is
        // checked and resolved BEFORE the generic `MAP_SHARED|PROT_WRITE`-on-a-file rejection
        // just below: unlike an ordinary file, a DRM device's `mmap()` has always genuinely
        // supported writable shared mappings in real Linux (that rejection describes an actual
        // limitation of THIS shim's generic file-backed-mapping path, not of DRM specifically).
        if !flags.contains(MapFlags::MAP_ANONYMOUS)
            && let Some(result) =
                self.try_dri_dumb_buffer_mmap(addr, len, &prot, &flags, fd, offset)
        {
            return result.map_err(Errno::from);
        }

        // Same rationale as the DRI check just above, for `memfd_create` fds: a real
        // `memfd_create` object has always genuinely supported writable shared mappings on real
        // Linux (that's its entire purpose -- anonymous shared memory for exactly this use case,
        // e.g. Wayland's `wl_shm.create_pool`), so it must be resolved before the generic
        // file-backed-mapping rejection below, which describes a real limitation of THIS shim's
        // ordinary-file path, not of memfd specifically.
        if !flags.contains(MapFlags::MAP_ANONYMOUS)
            && let Some(result) = self.try_memfd_mmap(addr, len, &flags, fd, offset)
        {
            return result.map_err(Errno::from);
        }

        // An ordinary file mapped WRITABLE and MAP_SHARED gets a real shared-memory object,
        // resolved before the rejection below -- see `try_shared_file_mmap` for why that
        // rejection was fatal rather than degrading for the callers that hit it. Read-only
        // shared mappings deliberately fall past this and keep their existing path.
        if !flags.contains(MapFlags::MAP_ANONYMOUS)
            && let Some(result) = self.try_shared_file_mmap(addr, len, &flags, fd, offset)
        {
            return result.map_err(Errno::from);
        }

        // MAP_SHARED is partially supported:
        // - Anonymous shared mappings are fully supported, including genuine cross-process
        //   sharing across fork(): backed by a real platform shared-memory object (see
        //   PageManagementProvider::create_shared_memory), re-mapped (not copied) into the
        //   child by Vmem::duplicate, so writes through either mapping are visible to the
        //   other -- on platforms that don't implement create_shared_memory, the underlying
        //   mmap call itself fails rather than silently degrading to copy-on-fork.
        // - File-backed shared mappings are read-only: writable permission is rejected
        //   upfront and cannot be added later via mprotect, because writes cannot be
        //   propagated back to the underlying file.
        if flags.contains(MapFlags::MAP_SHARED)
            && prot.contains(ProtFlags::PROT_WRITE)
            && !flags.contains(MapFlags::MAP_ANONYMOUS)
        {
            // This used to panic (`todo!()`), crashing the whole runner on any guest program
            // that mmaps a file `MAP_SHARED | PROT_WRITE` -- a fairly ordinary idiom (e.g.
            // Python's `mmap.mmap(fd, length, mmap.MAP_SHARED, mmap.PROT_WRITE)` for
            // memory-mapped file I/O, or SQLite/database libraries' write-back-mapped files).
            // `ENODEV` is what real Linux returns for "the underlying filesystem does not
            // support memory mapping" this way (see `mmap(2)`), which is an accurate description
            // of the actual limitation here.
            log_unsupported!("mmap MAP_SHARED|PROT_WRITE on a file-backed mapping");
            return Err(Errno::ENODEV);
        }

        if flags.intersects(
            MapFlags::MAP_32BIT
                | MapFlags::MAP_GROWSDOWN
                | MapFlags::MAP_LOCKED
                | MapFlags::MAP_NONBLOCK
                | MapFlags::MAP_SYNC
                | MapFlags::MAP_HUGETLB
                | MapFlags::MAP_HUGE_2MB
                | MapFlags::MAP_HUGE_1GB,
        ) {
            // Same rationale as above: don't panic on flag combinations we don't implement.
            log_unsupported!("mmap with flags {:?}", flags);
            return Err(Errno::EINVAL);
        }

        let aligned_len = align_up(len, PAGE_SIZE);
        if aligned_len == 0 {
            return Err(Errno::ENOMEM);
        }
        if offset.checked_add(aligned_len).is_none() {
            return Err(Errno::EOVERFLOW);
        }

        let suggested_addr = if addr == 0 { None } else { Some(addr) };
        let result = if flags.contains(MapFlags::MAP_ANONYMOUS) {
            self.do_mmap_anonymous(suggested_addr, aligned_len, prot, flags)
        } else {
            self.do_mmap_file(suggested_addr, aligned_len, prot, flags, fd, offset)
        };
        result.map_err(Errno::from)
    }

    /// Handle syscall `munmap`
    #[inline]
    pub(crate) fn sys_munmap(&self, addr: UserPtrMut<u8>, len: usize) -> Result<(), Errno> {
        let result = self.sys_munmap_raw(addr, len);
        // Mirrors `sys_mmap`'s own "returned"/traced-addr debug log (see its comment): without
        // this, no munmap event ever appears in a `LITEBOX_LOG=debug` trace, making it impossible
        // to correlate a later use-after-free's own faulting address against "what was this
        // memory's last known lifecycle event" -- confirmed a real, previously-undocumented gap
        // while investigating the mallocng `.meta=0` use-after-free (a group pointer's crashing
        // address had zero matches anywhere in an otherwise-complete debug trace).
        litebox_util_log::debug!(
            tid:% = self.tid.get(), host_tid:% = self.global.platform.host_debug_tid(),
            addr:% = addr.as_usize(), len:% = len, ok:% = result.is_ok();
            "sys_munmap"
        );
        if result.is_ok() {
            self.clear_file_mappings_for_range(addr.as_usize(), len);
        }
        result
    }

    /// Raw munmap without clearing file_mappings — used internally by the
    /// patching logic to avoid deadlocks (the patch path holds elf_patch_cache).
    #[inline]
    fn sys_munmap_raw(&self, addr: UserPtrMut<u8>, len: usize) -> Result<(), Errno> {
        litebox_common_linux::mm::sys_munmap(&self.process().pm(), addr, len)
    }

    /// Clear `file_mappings` entries for any segments that overlap the
    /// unmapped range, so that re-mapping the same file region will be
    /// re-patched instead of skipped.
    fn clear_file_mappings_for_range(&self, unmap_start: usize, unmap_len: usize) {
        let unmap_end = unmap_start.saturating_add(unmap_len);
        let mut cache = self.global.elf_patch_cache.lock();
        for ((pid, _), state) in cache.iter_mut() {
            // The unmapped range is an address in *this* process's address space; entries owned by
            // other processes describe unrelated address spaces (see [`ElfPatchKey`]).
            if *pid != self.pid.get() {
                continue;
            }
            state.file_mappings.retain(|&(vaddr, seg_len)| {
                let seg_end = vaddr.saturating_add(seg_len);
                seg_end <= unmap_start || vaddr >= unmap_end
            });
            state.patched_ranges.retain(|&(vaddr, seg_len)| {
                let seg_end = vaddr.saturating_add(seg_len);
                seg_end <= unmap_start || vaddr >= unmap_end
            });
        }
    }

    /// Handle syscall `mprotect`
    #[inline]
    pub(crate) fn sys_mprotect(
        &self,
        addr: UserPtrMut<u8>,
        len: usize,
        prot: ProtFlags,
    ) -> Result<(), Errno> {
        litebox_util_log::debug!(
            tid:% = self.tid.get(), addr:% = addr.as_usize(), len:% = len, prot:? = prot;
            "sys_mprotect: entry"
        );
        // Intercept transitions to PROT_EXEC: patch unpatched file mappings.
        if prot.contains(ProtFlags::PROT_EXEC) {
            let syscall_entry = self.global.platform.get_syscall_entry_point();
            if syscall_entry != 0 {
                self.maybe_patch_on_mprotect_exec(addr, len, syscall_entry);
            }
        }
        let result = self.sys_mprotect_raw(addr, len, prot);
        litebox_util_log::debug!(
            tid:% = self.tid.get(), ok:% = result.is_ok();
            "sys_mprotect: returned"
        );
        result
    }

    /// Raw mprotect without exec interception — used internally by the
    /// patching logic to avoid deadlocks (the patch path holds elf_patch_cache).
    #[inline]
    fn sys_mprotect_raw(
        &self,
        addr: UserPtrMut<u8>,
        len: usize,
        prot: ProtFlags,
    ) -> Result<(), Errno> {
        litebox_common_linux::mm::sys_mprotect(&self.process().pm(), addr, len, prot)
    }

    #[inline]
    pub(crate) fn sys_mremap(
        &self,
        old_addr: UserPtrMut<u8>,
        old_size: usize,
        new_size: usize,
        flags: MRemapFlags,
        new_addr: usize,
    ) -> Result<UserPtrMut<u8>, Errno> {
        let flags_for_log = alloc::format!("{flags:?}");
        let result = litebox_common_linux::mm::sys_mremap(
            &self.process().pm(),
            old_addr,
            old_size,
            new_size,
            flags,
            new_addr,
        );
        if let Err(e) = &result {
            // `sys_mremap` previously had no logging at all -- confirmed live as a real gap
            // via a genuine Weston `mremap()` failure (its pixman shadow-framebuffer growth)
            // that was completely invisible in `LITEBOX_LOG=debug` output, only surfacing
            // indirectly as Weston's own `wl_output.error` "failed mremap" event to its
            // client, which then cascaded into a GTK "cannot open display" failure with zero
            // syscall-level evidence pointing back at the actual `mremap()` call. Only log the
            // failure path (mirroring `sys_brk`'s pattern just below); a successful mremap is
            // already visible via the ordinary `mmap`/`mprotect` traces around it.
            litebox_util_log::debug!(
                tid:% = self.tid.get(), old_addr:% = old_addr.as_usize(), old_size:% = old_size,
                new_size:% = new_size, flags:% = flags_for_log, new_addr:% = new_addr, err:? = e;
                "sys_mremap: failed"
            );
        }
        result
    }

    /// Handle syscall `brk`
    #[inline]
    pub(crate) fn sys_brk(&self, addr: UserPtrMut<u8>) -> Result<usize, Errno> {
        let result = litebox_common_linux::mm::sys_brk(&self.process().pm(), addr);
        // Temporary (see FINDINGS.txt PASS 128): trace every brk() call's requested and
        // returned break address, mirroring PASS 48's sys_mmap trace above, to determine
        // whether the ~10.2MB region containing the mallocng meta-slot bug (BUG B,
        // alloc_base=0x9c0000) is established via brk() rather than mmap().
        if let Ok(r) = &result {
            litebox_util_log::debug!(
                addr:% = addr.as_usize(), returned:% = r;
                "sys_brk: returned"
            );
        }
        result
    }

    /// Handle syscall `madvise`
    #[inline]
    pub(crate) fn sys_madvise(
        &self,
        addr: UserPtrMut<u8>,
        len: usize,
        advice: litebox_common_linux::MadviseBehavior,
    ) -> Result<(), Errno> {
        litebox_common_linux::mm::sys_madvise(&self.process().pm(), addr, len, advice)
    }

    // ── Runtime ELF syscall patching ─────────────────────────────────────

    /// Check all tracked file mappings for unpatched regions that overlap the
    /// mprotect range. If found, run the runtime rewriter before the region
    /// becomes executable.
    fn maybe_patch_on_mprotect_exec(&self, addr: UserPtrMut<u8>, len: usize, syscall_entry: usize) {
        let mprotect_start = addr.as_usize();
        let mprotect_end = mprotect_start.saturating_add(len);

        // Find unpatched file mappings that overlap this mprotect range.
        // We collect (fd, vaddr, seg_len, file_offset) to avoid holding
        // the lock while patching.
        let to_patch: alloc::vec::Vec<(i32, usize, usize)> = {
            let cache = self.global.elf_patch_cache.lock();
            let mut result = alloc::vec::Vec::new();
            for (&(pid, fd), state) in cache.iter() {
                // Only this process's own entries describe this address space; another process's
                // absolute `file_mappings` addresses are meaningless here (see [`ElfPatchKey`]).
                if pid != self.pid.get() || state.pre_patched {
                    continue;
                }
                for &(seg_start, seg_len) in &state.file_mappings {
                    let seg_end = seg_start.saturating_add(seg_len);
                    // Check overlap with the mprotect range.
                    if seg_start < mprotect_end && seg_end > mprotect_start {
                        result.push((fd, seg_start, seg_len));
                    }
                }
            }
            result
        };

        // A single mprotect range should only overlap mappings from one fd
        // (a given vaddr range is backed by at most one file at a time).
        if to_patch.len() > 1 {
            let fds: BTreeSet<i32> = to_patch.iter().map(|(fd, _, _)| *fd).collect();
            if fds.len() > 1 {
                litebox_util_log::warn!(
                    addr:? = mprotect_start, len:? = len, fds:? = fds;
                    "mprotect +EXEC range overlaps file mappings from multiple fds"
                );
            }
        }

        for (fd, seg_start, seg_len) in to_patch {
            // Clamp to the intersection of the tracked mapping and the
            // mprotect range — only patch the portion becoming executable.
            // Re-running the rewriter on already-patched bytes is safe,
            // so we don't need to track sub-range overlaps precisely.
            let seg_end = seg_start.saturating_add(seg_len);
            let patch_start = seg_start.max(mprotect_start);
            let patch_end = seg_end.min(mprotect_end);
            let patch_len = patch_end.saturating_sub(patch_start);
            if patch_len == 0 {
                continue;
            }
            let mapped_addr = UserPtrMut::<u8>::from_usize(patch_start);
            // AGENTS.md pass 260 follow-up: the direct mmap(PROT_EXEC)-time diagnostic missed
            // the actual weston crash addresses entirely -- this is the OTHER route a mapping
            // gains PROT_EXEC (an mmap(PROT_READ) followed later by mprotect(PROT_EXEC), the
            // classic dynamic-linker lazy-mapping idiom), so track it here too.
            let path = self.files.borrow().lookup_fd_path(fd as usize);
            litebox_util_log::debug!(
                path:? = path, start:% = patch_start, len:% = patch_len;
                "diag-exec-mmap: tracking via mprotect(PROT_EXEC) for future crash-address correlation"
            );
            self.maybe_patch_exec_segment(mapped_addr, patch_len, fd, syscall_entry, None);
        }
    }

    /// Initialize ELF patch state for an fd on its first mmap.
    ///
    /// Reads the ELF header to determine the trampoline address (page-aligned
    /// end of the highest PT_LOAD segment) and checks the file tail for the
    /// trampoline magic to determine if it's pre-patched.
    ///
    /// For ET_DYN binaries (PIE/shared libs), virtual addresses in program
    /// headers are relative to a base address chosen at load time. We derive
    /// the base from the caller's mapping: `base = mapped_addr - p_vaddr` of
    /// the segment being mapped. The `file_offset` parameter identifies which
    /// segment is being mapped so we can look up its `p_vaddr`.
    ///
    /// x86_64 only: assumes 64-bit ELF layout and program header offsets, and
    /// `litebox_syscall_rewriter::patch_code_segment` itself only recognizes x86_64 `syscall`
    /// opcodes -- on any other architecture, scanning a guest binary's actual machine code for
    /// that specific byte pattern risks a false-positive match (e.g. `0f 05` occurring inside
    /// legitimate aarch64 instructions) and corrupting the guest's executable memory with a
    /// bogus patch attempt. Skip this entire subsystem outside x86_64; the seccomp+SIGSYS
    /// fallback path handles every syscall correctly there regardless.
    #[cfg(not(target_arch = "x86_64"))]
    fn init_elf_patch_state(&self, _fd: i32, _mapped_addr: usize, _file_offset: usize) {}

    #[cfg(target_arch = "x86_64")]
    fn init_elf_patch_state(&self, fd: i32, mapped_addr: usize, file_offset: usize) {
        // Quick check: skip if already initialized.
        if self
            .global
            .elf_patch_cache
            .lock()
            .contains_key(&self.elf_patch_key(fd))
        {
            return;
        }
        // NOTE (not yet fixed): `ElfPatchKey` is `(pid, fd)` and nothing re-keys a parent's
        // entries onto the child's pid at `fork()`. So a forked child, whose address space
        // already holds the parent's ALREADY-PATCHED code copied byte for byte, looks its own pid
        // up, misses, and re-initializes patch state from scratch -- computing a fresh
        // `trampoline_addr` while the copied code still jumps to the parent's. Worth revisiting.

        // Read the ELF header (64 bytes for Elf64).
        let mut ehdr_buf = [0u8; core::mem::size_of::<FileHeader64<LittleEndian>>()];
        match self.sys_read(fd, &mut ehdr_buf, Some(0)) {
            Ok(n) if n == ehdr_buf.len() => {}
            _ => return, // Not readable or short read, skip
        }

        // Parse as typed ELF64 header.
        let Ok((ehdr, _)) = object::from_bytes::<FileHeader64<LittleEndian>>(&ehdr_buf) else {
            return;
        };

        // Verify ELF magic
        if &ehdr.e_ident.magic != b"\x7fELF" {
            return;
        }

        let e_type = ehdr.e_type.get(ENDIAN);
        let e_phoff: usize = ehdr.e_phoff.get(ENDIAN).trunc();
        let e_phentsize = ehdr.e_phentsize.get(ENDIAN) as usize;
        let e_phnum = ehdr.e_phnum.get(ENDIAN) as usize;

        // Validate e_phentsize: must be at least sizeof(Elf64_Phdr).
        if e_phentsize < core::mem::size_of::<ProgramHeader64<LittleEndian>>() {
            return;
        }

        // Read program headers.
        let Some(phdrs_size) = e_phentsize.checked_mul(e_phnum) else {
            return;
        };
        if phdrs_size == 0 || phdrs_size > 0x10000 {
            return; // Sanity check
        }
        let mut phdrs_buf = alloc::vec![0u8; phdrs_size];
        match self.sys_read(fd, &mut phdrs_buf, Some(e_phoff)) {
            Ok(n) if n == phdrs_buf.len() => {}
            _ => return,
        }

        // Find highest PT_LOAD end (p_vaddr + p_memsz) and compute base_addr
        // by matching the segment whose p_offset corresponds to file_offset.
        let mut max_load_end: u64 = 0;
        let mut base_addr: Option<usize> = None;
        for i in 0..e_phnum {
            let ph_bytes = &phdrs_buf[i * e_phentsize..][..e_phentsize];
            let Ok((ph, _)) = object::from_bytes::<ProgramHeader64<LittleEndian>>(ph_bytes) else {
                continue;
            };
            if ph.p_type.get(ENDIAN) != PT_LOAD {
                continue;
            }
            let p_offset: usize = ph.p_offset.get(ENDIAN).trunc();
            let p_vaddr = ph.p_vaddr.get(ENDIAN);
            let p_memsz = ph.p_memsz.get(ENDIAN);
            let Some(end) = p_vaddr.checked_add(p_memsz) else {
                litebox_util_log::warn!(
                    p_vaddr:? = p_vaddr, p_memsz:? = p_memsz;
                    "PT_LOAD p_vaddr + p_memsz overflow, skipping segment"
                );
                continue;
            };
            if end > max_load_end {
                max_load_end = end;
            }
            // Match segment by page-aligned file offset to derive base address.
            if base_addr.is_none()
                && align_down(p_offset, PAGE_SIZE) == align_down(file_offset, PAGE_SIZE)
            {
                base_addr = Some(mapped_addr.wrapping_sub(p_vaddr.trunc()));
            }
        }

        if max_load_end == 0 {
            return; // No PT_LOAD segments
        }

        // Check if file is pre-patched by reading the last 32 bytes for magic
        let (pre_patched, tramp_file_offset, tramp_vaddr, tramp_file_size) =
            self.check_trampoline_magic(fd);

        // Compute the trampoline virtual address.
        // - Pre-patched: use the exact address from the trampoline header (the
        //   code already contains JMPs there, so we MUST map at this address).
        // - Unpatched: place it just past the highest PT_LOAD end (this is just
        //   a hint — validated by the ±2GB distance check with trap fallback).
        // For ET_DYN, virtual addresses are relative to the load base.
        let trampoline_vaddr = if pre_patched {
            if e_type == ET_DYN {
                let Some(base) = base_addr else {
                    panic!(
                        "fatal: pre-patched ET_DYN binary but cannot determine load base address"
                    );
                };
                let vaddr: usize = tramp_vaddr.trunc();
                base + vaddr
            } else {
                tramp_vaddr.trunc()
            }
        } else {
            let base = if e_type == ET_DYN {
                base_addr.unwrap_or(mapped_addr)
            } else {
                0
            };
            let max_end: usize = max_load_end.trunc();
            base + max_end.next_multiple_of(PAGE_SIZE)
        };

        litebox_util_log::debug!(
            guest_tid:? = self.sys_gettid(),
            fd:? = fd, mapped_addr:? = mapped_addr, base_addr:? = base_addr,
            pre_patched:? = pre_patched, tramp_file_size:? = tramp_file_size,
            trampoline_vaddr:? = trampoline_vaddr;
            "init_elf_patch_state: computed trampoline"
        );

        // Insert under lock (re-check for races).
        let mut cache = self.global.elf_patch_cache.lock();
        cache
            .entry(self.elf_patch_key(fd))
            .or_insert(ElfPatchState {
                pre_patched,
                trampoline_file_offset: tramp_file_offset,
                trampoline_file_size: tramp_file_size.trunc(),
                trampoline_addr: trampoline_vaddr,
                trampoline_cursor: 0,
                trampoline_mapped: false,
                trampoline_mapped_len: 0,
                runtime_patches_committed: false,
                file_mappings: BTreeSet::new(),
                patched_ranges: BTreeSet::new(),
            });
    }

    /// Check if a file has the LITEBOX trampoline magic at its tail.
    /// Returns (is_pre_patched, file_offset, vaddr, trampoline_size).
    ///
    /// Parses the SAME `TrampolineHeader64` wire layout
    /// `litebox_common_linux::loader::parse_trampoline` itself parses (via the same
    /// `zerocopy::FromBytes` derive), rather than a second, independently-maintained
    /// hand-rolled `from_le_bytes` decoder for the identical bytes -- two divergent parsers for
    /// one on-disk format is how they'd silently disagree if either one were ever updated alone.
    #[cfg(target_arch = "x86_64")]
    fn check_trampoline_magic(&self, fd: i32) -> (bool, u64, u64, u64) {
        use litebox_common_linux::loader::TrampolineHeader64;
        use zerocopy::FromBytes as _;

        const HEADER_SIZE: usize = core::mem::size_of::<TrampolineHeader64>();
        let Ok(stat) = self.sys_fstat(fd) else {
            return (false, 0, 0, 0);
        };
        #[cfg(target_arch = "aarch64")]
        let Ok(file_size) = usize::try_from(stat.st_size) else {
            return (false, 0, 0, 0);
        };
        #[cfg(not(target_arch = "aarch64"))]
        let file_size = stat.st_size;
        if file_size < HEADER_SIZE {
            return (false, 0, 0, 0);
        }
        let mut tail = [0u8; HEADER_SIZE];
        let read_offset = file_size - HEADER_SIZE;
        match self.sys_read(fd, &mut tail, Some(read_offset)) {
            Ok(n) if n == HEADER_SIZE => {}
            _ => return (false, 0, 0, 0),
        }
        if &tail[0..8] != litebox_syscall_rewriter::TRAMPOLINE_MAGIC {
            return (false, 0, 0, 0);
        }
        let Ok(header) = TrampolineHeader64::read_from_bytes(&tail) else {
            return (false, 0, 0, 0);
        };
        (true, header.file_offset, header.vaddr, header.trampoline_size)
    }

    /// Probe for a free address within JMP rel32 range (`0x7FFF_0000`) of a code segment
    /// (`code_addr..code_end`), by trying real `MAP_FIXED_NOREPLACE` attempts at
    /// exponentially-increasing offsets on alternating sides of `preferred_addr` -- the
    /// ELF-computed "just past this file's last `PT_LOAD`" hint that a plain
    /// `MAP_FIXED_NOREPLACE` at that exact address has already failed for (some other mapping
    /// occupies it; common in a cross-process-fork child, whose adopted VMA layout starts far
    /// denser than a freshly-booted process's).
    ///
    /// Exists because `Vmem::get_unmmaped_area`'s own "let the VM choose" fallback (what the
    /// caller reaches for next if this returns `Err`) has NO notion of "nearby": a `suggested_
    /// address` that is occupied and not `MAP_FIXED` is silently ignored, and the fully generic
    /// top-down/gap search that runs instead returns the first free gap ANYWHERE in the guest's
    /// address space, which can land billions of bytes away from `preferred_addr` with nothing
    /// to stop it (live-diagnosed 2026-09-17: a freshly cross-process-forked child's very first
    /// `execve`, e.g. plain `mkdir`, landed a library's trampoline ~140 TB from its code segment,
    /// `distance > 0x7FFF_0000`, triggering `apply_trap_fallback` -- which poisons every `syscall`
    /// in that segment to a crash trap -- and the guest died the first time it actually executed
    /// one). Deliberately a LOCAL probe scoped to just this one caller, not a change to
    /// `get_unmmaped_area` itself (used by every `mmap()` in the system): each candidate is a
    /// real, cheap-on-failure syscall (no side effects beyond the attempt itself), and the
    /// exponential step (doubling each round, both directions) bounds the total probe count to
    /// `PROBE_ROUNDS * 2` regardless of how far a usable gap turns out to be, rather than a linear
    /// scan that could need hundreds of thousands of steps to cross a multi-GB packed region.
    fn probe_nearby_trampoline_slot(
        &self,
        preferred_addr: usize,
        code_addr: usize,
        code_end: usize,
        size: usize,
    ) -> Result<UserPtrMut<u8>, MappingError> {
        const JMP_REL32_RANGE: usize = 0x7FFF_0000;
        const PROBE_ROUNDS: u32 = 24;
        let mut step = size.next_power_of_two().max(PAGE_SIZE);
        for _ in 0..PROBE_ROUNDS {
            for candidate in [
                preferred_addr.checked_add(step),
                preferred_addr.checked_sub(step),
            ]
            .into_iter()
            .flatten()
            {
                // Stay within JMP rel32 range of BOTH ends of the code segment -- matches the
                // real check the caller applies to whatever address this returns.
                if candidate.abs_diff(code_addr) > JMP_REL32_RANGE
                    || candidate.abs_diff(code_end) > JMP_REL32_RANGE
                {
                    continue;
                }
                if let Ok(addr) = self.do_mmap_anonymous(
                    Some(candidate),
                    size,
                    ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                    MapFlags::MAP_ANONYMOUS | MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED_NOREPLACE,
                ) {
                    return Ok(addr);
                }
            }
            let Some(next_step) = step.checked_mul(2) else {
                break;
            };
            step = next_step;
        }
        Err(MappingError::OutOfMemory)
    }

    /// Apply the trap fallback to a mapped code segment: replace all `syscall`
    /// instructions with traps (`ICEBP;HLT`), then restore RX.
    ///
    /// If `already_rw` is true, the segment is assumed to already be writable
    /// and the initial mprotect RW is skipped.
    ///
    /// Panics on infrastructure failures (mprotect/read/write/disassembly).
    fn apply_trap_fallback(&self, mapped_addr: UserPtrMut<u8>, len: usize, already_rw: bool) {
        if !already_rw {
            self.sys_mprotect_raw(
                mapped_addr,
                len,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            )
            .expect("fatal: failed to mprotect code segment RW for trap fallback");
        }

        // Read, patch using the rewriter (proper disassembly), write back.
        let Some(code_owned) = mapped_addr.to_owned_slice::<Platform>(len) else {
            panic!("fatal: failed to read code segment for trap fallback");
        };
        let mut code_buf = code_owned.into_vec();
        let code_vaddr = mapped_addr.as_usize() as u64;
        let count = litebox_syscall_rewriter::trap_all_syscalls_in_code(&mut code_buf, code_vaddr)
            .unwrap_or_else(|e| {
                panic!("fatal: failed to disassemble code segment for trap fallback: {e:?}");
            });
        if count > 0 {
            litebox_util_log::warn!(
                count:? = count, addr:? = mapped_addr.as_usize(), len:? = len;
                "applied trap fallback to syscall instructions"
            );
        }
        assert!(
            mapped_addr
                .copy_from_slice::<Platform>(0, &code_buf)
                .is_some(),
            "fatal: failed to write trap bytes back to code segment"
        );

        // Restore RX.
        self.sys_mprotect_raw(
            mapped_addr,
            len,
            ProtFlags::PROT_READ | ProtFlags::PROT_EXEC,
        )
        .expect("fatal: failed to restore code segment to RX after trap fallback");
    }

    /// Patch an executable segment in-place after it has been mapped.
    ///
    /// For pre-patched binaries: maps the trampoline from the file and writes
    /// the syscall entry point.
    /// For unpatched binaries: calls `patch_code_segment()` to rewrite syscall
    /// instructions and places the generated stubs in the trampoline region.
    ///
    /// Returns `true` on success or non-fatal skip. Returns `false` when a
    /// pre-patched binary's trampoline could not be set up — the caller must
    /// fail the mapping because the code already contains JMPs to the
    /// trampoline address.
    fn maybe_patch_exec_segment(
        &self,
        mapped_addr: UserPtrMut<u8>,
        len: usize,
        fd: i32,
        syscall_entry: usize,
        file_offset: Option<usize>,
    ) -> bool {
        // Initialize patch state if this is the first mmap for this fd.
        // Typically the first mapping is at offset 0 (the ELF header), but
        // some loaders may map an executable segment at a non-zero offset first.
        if !self
            .global
            .elf_patch_cache
            .lock()
            .contains_key(&self.elf_patch_key(fd))
        {
            self.init_elf_patch_state(fd, mapped_addr.as_usize(), file_offset.unwrap_or(0));
        }

        // This lock guards the elf_patch_cache and is held for the entire
        // patching operation. In practice this is fine because the dynamic
        // linker loads shared libraries sequentially.
        let mut cache = self.global.elf_patch_cache.lock();
        let Some(state) = cache.get_mut(&self.elf_patch_key(fd)) else {
            return true; // No patch state — not an ELF we're tracking
        };

        if state.pre_patched {
            // Pre-patched binary: map the trampoline data from the file.
            if !state.trampoline_mapped && state.trampoline_file_size > 0 {
                let tramp_addr = state.trampoline_addr;
                let tramp_len = align_up(state.trampoline_file_size, PAGE_SIZE);

                // Allocate RW region at the trampoline address. Use MAP_FIXED
                // because the code already contains JMPs to this exact address
                // and we MUST map here. The region may already be reserved as
                // PROT_NONE by the ElfLoader's reserve() call, which would
                // cause MAP_FIXED_NOREPLACE to fail with EEXIST.
                let alloc_result = self.do_mmap_anonymous(
                    Some(tramp_addr),
                    tramp_len,
                    ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                    MapFlags::MAP_ANONYMOUS | MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED,
                );
                let Ok(alloc_ptr) = alloc_result else {
                    return false;
                };
                let actual_addr = alloc_ptr.as_usize();
                if actual_addr != tramp_addr {
                    let _ =
                        self.sys_munmap_raw(UserPtrMut::<u8>::from_usize(actual_addr), tramp_len);
                    return false;
                }

                // Read trampoline data from the file.
                let mut tramp_data = alloc::vec![0u8; state.trampoline_file_size];
                let file_off = state.trampoline_file_offset.trunc();
                let tramp_ptr = UserPtrMut::<u8>::from_usize(tramp_addr);
                match self.sys_read(fd, &mut tramp_data, Some(file_off)) {
                    Ok(n) if n == tramp_data.len() => {}
                    _ => {
                        let _ = self.sys_munmap_raw(tramp_ptr, tramp_len);
                        return false;
                    }
                }

                // Write syscall entry point to the first 8 bytes.
                if tramp_data.len() >= 8 {
                    tramp_data[..8].copy_from_slice(&syscall_entry.to_le_bytes());
                }

                // Write to the mapped region.
                if tramp_ptr
                    .copy_from_slice::<Platform>(0, &tramp_data)
                    .is_none()
                {
                    let _ = self.sys_munmap_raw(tramp_ptr, tramp_len);
                    return false;
                }

                // Protect as RX immediately.
                if let Err(err) = self.sys_mprotect_raw(
                    tramp_ptr,
                    tramp_len,
                    ProtFlags::PROT_READ | ProtFlags::PROT_EXEC,
                ) {
                    litebox_util_log::error!(
                        tramp_addr:% = tramp_ptr.as_usize(),
                        tramp_len:% = tramp_len,
                        errno:? = err;
                        "diag-tramp-mprotect-fail: pre-patched trampoline mprotect(RX) failed, tearing down via munmap -- syscall rewriting for this binary is now BROKEN"
                    );
                    let _ = self.sys_munmap_raw(tramp_ptr, tramp_len);
                    return false;
                }

                state.trampoline_mapped = true;
                state.trampoline_mapped_len = tramp_len;
            }
            return true;
        }

        // ── Runtime patching path (unpatched binaries) ───────────────

        // Allocate the trampoline region if not yet done.
        let addr_usize = mapped_addr.as_usize();
        if !state.trampoline_mapped {
            let tramp_addr = state.trampoline_addr;

            // Size the initial allocation from a cheap upper bound on how many `syscall`
            // (`0F 05`) byte pairs this segment can possibly contain, rather than a flat
            // `PAGE_SIZE` guess. A real container-image binary (e.g. GNU bash, vs. the
            // busybox this was originally sized against) routinely needs far more than one
            // page of stubs: undersizing here used to fall through to the `trampoline_mapped_len`
            // "extend" path below, which only ever tries ONE fixed, exactly-adjacent address via
            // `MAP_FIXED_NOREPLACE` with no fallback -- unlike this initial allocation's own
            // try-fixed-then-let-the-VM-choose fallback a few lines down. Any unrelated mapping
            // already occupying that single adjacent address (common; nothing reserves it) made
            // the extend fail outright, which nukes EVERY syscall in the whole segment to an
            // `ICEBP;HLT` crash trap (`apply_trap_fallback`) -- including the ones that were
            // otherwise perfectly patchable -- so the guest died on the first syscall it ever
            // executed after load (confirmed live: `docker.io/edgelevel/alpine-xfce-vnc`'s `/bin/sh`
            // == bash, SIGILL within 3s of exec, 480 syscalls trap-poisoned by one failed 4KiB
            // extension). Counting the byte pairs is the same sound-upper-bound technique
            // `litebox_syscall_rewriter::patch_code_segment`'s own fast-reject scan already relies
            // on: a real `syscall` is always exactly `0F 05` with no prefix that changes those
            // bytes, so this can only OVER-count (data or another instruction's encoding
            // containing that pair), never under-count, and sizing off an over-count is safe.
            let syscall_upper_bound = mapped_addr
                .to_owned_slice::<Platform>(len)
                .map(|owned| {
                    let buf = owned.into_vec();
                    buf.windows(2)
                        .filter(|w| w[0] == 0x0F && w[1] == 0x05)
                        .count()
                })
                .unwrap_or(0);
            // Per-syscall stub upper bound: the fixed lea+jmp+jmp-back sequence
            // (`hook_syscalls_in_section`) is 18 bytes, plus up to `SYSCALL_CONTEXT_INSTRUCTIONS`
            // (8) re-encoded instructions of at most 15 bytes (x86-64's own max instruction
            // length) each on the richer pre/post-syscall paths -- 128 comfortably covers that
            // with headroom, and only pads address space (never committed memory) if it
            // overshoots.
            const MAX_STUB_BYTES_PER_SYSCALL: usize = 128;
            const TRAMPOLINE_ENTRY_BYTES: usize = 8;
            // Capped: `syscall_upper_bound` counts raw `0F 05` byte pairs anywhere in the
            // mapping, including non-code data (rodata sharing the segment, or an unrelated
            // byte pair inside another instruction's encoding) -- real code never approaches
            // this density, so a huge count here means the segment is huge and mostly NOT
            // syscalls, not that it genuinely needs gigabytes of trampoline. 4 MiB covers over
            // 32,000 real syscall sites (every real-world binary seen so far needs under 1,000)
            // while bounding the one-time address-space/commit cost for a large, data-heavy
            // mapping. A segment that legitimately needs more than this still has the existing
            // `trampoline_mapped_len`-extension path as a backstop, unchanged.
            const MAX_INITIAL_TRAMPOLINE_SIZE: usize = 4 * 1024 * 1024;
            let initial_tramp_size = align_up(
                TRAMPOLINE_ENTRY_BYTES
                    + syscall_upper_bound.saturating_mul(MAX_STUB_BYTES_PER_SYSCALL),
                PAGE_SIZE,
            )
            .max(PAGE_SIZE)
            .min(MAX_INITIAL_TRAMPOLINE_SIZE);

            // Try MAP_FIXED_NOREPLACE first — works when the preferred
            // trampoline address is available. If that fails, probe nearby
            // addresses within JMP rel32 range (see `probe_nearby_trampoline_slot`'s
            // own doc comment for why this step exists: the generic "let the VM
            // manager choose" fallback below has no notion of "nearby" and can
            // land anywhere in the guest's address space). Only if EVERY nearby
            // candidate is also occupied does this fall through to that fully
            // generic choice, still re-validated against the JMP rel32 range below.
            let far_end_hint = addr_usize.saturating_add(len);
            let actual_addr = self
                .do_mmap_anonymous(
                    Some(tramp_addr),
                    initial_tramp_size,
                    ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                    MapFlags::MAP_ANONYMOUS | MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED_NOREPLACE,
                )
                .or_else(|_| {
                    self.probe_nearby_trampoline_slot(
                        tramp_addr,
                        addr_usize,
                        far_end_hint,
                        initial_tramp_size,
                    )
                })
                .or_else(|_| {
                    self.do_mmap_anonymous(
                        None,
                        initial_tramp_size,
                        ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                        MapFlags::MAP_ANONYMOUS | MapFlags::MAP_PRIVATE,
                    )
                });
            let Ok(actual_addr_ptr) = actual_addr else {
                litebox_util_log::warn!("failed to allocate trampoline region");
                self.apply_trap_fallback(mapped_addr, len, false);
                return true;
            };
            let actual_addr = actual_addr_ptr.as_usize();

            // Verify the trampoline is within JMP rel32 range (+-2GB) of the
            // entire code segment, not just its start.
            let far_end = addr_usize.saturating_add(len);
            let distance = actual_addr
                .abs_diff(addr_usize)
                .max(actual_addr.abs_diff(far_end));
            if distance > 0x7FFF_0000 {
                litebox_util_log::warn!(
                    distance:? = distance;
                    "trampoline too far from code segment, skipping patching"
                );
                let _ = self.sys_munmap_raw(
                    UserPtrMut::<u8>::from_usize(actual_addr),
                    initial_tramp_size,
                );
                self.apply_trap_fallback(mapped_addr, len, false);
                return true;
            }

            state.trampoline_addr = actual_addr;

            // Write the 8-byte syscall entry point at the start.
            let entry_ptr = UserPtrMut::<u8>::from_usize(actual_addr);
            if entry_ptr
                .copy_from_slice::<Platform>(0, &syscall_entry.to_le_bytes())
                .is_none()
            {
                litebox_util_log::warn!("failed to write syscall entry point to trampoline");
                let _ = self.sys_munmap_raw(
                    UserPtrMut::<u8>::from_usize(actual_addr),
                    initial_tramp_size,
                );
                self.apply_trap_fallback(mapped_addr, len, false);
                return true;
            }
            state.trampoline_cursor = 8; // stubs start after the 8-byte entry
            state.trampoline_mapped = true;
            state.trampoline_mapped_len = initial_tramp_size;
        }

        // Performance guard: skip if this exact range was already patched.
        let mapping_key = (mapped_addr.as_usize(), len);
        if state.patched_ranges.contains(&mapping_key) {
            return true;
        }
        state.patched_ranges.insert(mapping_key);

        let restore_trampoline_rx = |task: &Self, state: &ElfPatchState| {
            if state.trampoline_mapped_len > 0 {
                let _ = task.sys_mprotect_raw(
                    UserPtrMut::<u8>::from_usize(state.trampoline_addr),
                    state.trampoline_mapped_len,
                    ProtFlags::PROT_READ | ProtFlags::PROT_EXEC,
                );
            }
        };

        // Make the trampoline RW for writing stubs. A failure here (e.g. transient memory
        // pressure, or the trampoline region having been unmapped/reprotected out from under us)
        // is not fatal to the guest: fall back to trap-based syscall interception for this
        // segment instead, matching every other recoverable failure path in this function (e.g.
        // the trampoline-allocation failure above).
        if state.trampoline_mapped_len > 0
            && self
                .sys_mprotect_raw(
                    UserPtrMut::<u8>::from_usize(state.trampoline_addr),
                    state.trampoline_mapped_len,
                    ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                )
                .is_err()
        {
            litebox_util_log::warn!(
                "failed to mprotect trampoline to RW, falling back to trap patching"
            );
            self.apply_trap_fallback(mapped_addr, len, false);
            return true;
        }
        if self
            .sys_mprotect_raw(
                mapped_addr,
                len,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            )
            .is_err()
        {
            litebox_util_log::warn!(
                "failed to mprotect code segment to RW for patching, falling back to trap patching"
            );
            restore_trampoline_rx(self, state);
            self.apply_trap_fallback(mapped_addr, len, false);
            return true;
        }

        // Read the mapped code into a buffer, patch it, write back.
        let Some(code_owned) = mapped_addr.to_owned_slice::<Platform>(len) else {
            litebox_util_log::warn!(
                "failed to read code segment for patching, falling back to trap patching"
            );
            let rw_ok = self
                .sys_mprotect_raw(
                    mapped_addr,
                    len,
                    ProtFlags::PROT_READ | ProtFlags::PROT_EXEC,
                )
                .is_ok();
            restore_trampoline_rx(self, state);
            if rw_ok {
                self.apply_trap_fallback(mapped_addr, len, false);
            }
            return true;
        };
        let mut code_buf = code_owned.into_vec();
        let original_code = code_buf.clone();

        let code_vaddr = addr_usize as u64;
        let trampoline_write_vaddr = (state.trampoline_addr + state.trampoline_cursor) as u64;
        let syscall_entry_addr = state.trampoline_addr as u64;

        // Rewrite only the parts of this mapping that hold CODE, and scan each of them once per
        // file rather than once per mapping.
        //
        // Two separate things, both forced by measurement, both about the same 130 MB library:
        //
        // * WHAT to patch. A `PROT_EXEC` mapping is not all code -- `libLLVM`'s first `PT_LOAD` is
        //   `RX` and holds `.dynsym`, `.gnu.version*` and 42 MB of `.rodata` next to `.text`.
        //   Patching all of it corrupted the symbol tables and broke every mesa consumer; see
        //   `litebox_syscall_rewriter::executable_section_file_ranges` for the full chain.
        // * HOW OFTEN to scan. The disassembly is a pure function of the bytes, so it is cached per
        //   `(file, span)` and reused by every later mapping of that span, in any process; see
        //   `litebox_syscall_rewriter::SegmentScanTemplate`.
        //
        // When the file's code ranges cannot be determined (no section headers, unreadable, not an
        // ELF64) this falls back to treating the whole mapping as code -- the pre-existing
        // behaviour, no worse than before, and still correct for the ordinary case where the
        // mapping IS just a text segment.
        let map_file_start = file_offset.unwrap_or(0) as u64;
        let map_file_end = map_file_start.saturating_add(len as u64);
        let mut spans: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();
        match self.executable_file_ranges(fd) {
            Some(ranges) => {
                for range in ranges.iter() {
                    let start = range.start.max(map_file_start);
                    let end = range.end.min(map_file_end);
                    if start < end {
                        spans.push((
                            (start - map_file_start) as usize,
                            (end - map_file_start) as usize,
                        ));
                    }
                }
            }
            None => spans.push((0, len)),
        }

        let mut all_stubs: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        let mut all_skipped: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
        let mut patch_err = None;
        for (span_start, span_end) in spans {
            let span_len = span_end - span_start;
            let scan_key =
                self.segment_scan_key(fd, (map_file_start as usize) + span_start, span_len);
            let cached = scan_key.and_then(|key| {
                self.global
                    .segment_scan_cache
                    .lock()
                    .get(&key)
                    .map(alloc::sync::Arc::clone)
            });
            let template = match cached {
                Some(template) => Ok(template),
                None => litebox_syscall_rewriter::scan_code_segment(&code_buf[span_start..span_end])
                    .map(|scanned| {
                        let scanned = alloc::sync::Arc::new(scanned);
                        if let Some(key) = scan_key {
                            self.global
                                .segment_scan_cache
                                .lock()
                                .insert(key, alloc::sync::Arc::clone(&scanned));
                        }
                        scanned
                    }),
            };
            let outcome = template.and_then(|template| {
                litebox_syscall_rewriter::patch_code_segment_scanned(
                    &template,
                    &mut code_buf[span_start..span_end],
                    code_vaddr + span_start as u64,
                    trampoline_write_vaddr + all_stubs.len() as u64,
                    syscall_entry_addr,
                )
            });
            match outcome {
                Ok((stubs, skipped_addrs)) => {
                    all_stubs.extend_from_slice(&stubs);
                    all_skipped.extend_from_slice(&skipped_addrs);
                }
                Err(e) => {
                    patch_err = Some(e);
                    break;
                }
            }
        }
        let patch_result = match patch_err {
            Some(e) => Err(e),
            None => {
                if !all_skipped.is_empty() {
                    litebox_util_log::warn!(
                        count:? = all_skipped.len(), addrs:? = all_skipped;
                        "syscall instruction(s) could not be patched"
                    );
                }
                Ok(all_stubs)
            }
        };
        match patch_result {
            Ok(stubs) if !stubs.is_empty() => {
                let Some(new_cursor) = state.trampoline_cursor.checked_add(stubs.len()) else {
                    litebox_util_log::warn!("trampoline cursor overflow");
                    self.apply_trap_fallback(mapped_addr, len, true);
                    restore_trampoline_rx(self, state);
                    return true;
                };
                let tramp_pages_needed = align_up(new_cursor, PAGE_SIZE);
                if tramp_pages_needed > state.trampoline_mapped_len {
                    let extra_start = state.trampoline_addr + state.trampoline_mapped_len;
                    let extra_len = tramp_pages_needed - state.trampoline_mapped_len;
                    litebox_util_log::debug!(
                        extra_start:% = format_args!("{extra_start:#x}"),
                        extra_end:% = format_args!("{:#x}", extra_start + extra_len),
                        extra_len:% = extra_len,
                        trampoline_addr:% = format_args!("{:#x}", state.trampoline_addr),
                        trampoline_mapped_len:% = state.trampoline_mapped_len;
                        "diag-tramp-extend: about to extend trampoline region"
                    );
                    let extend_result = self.do_mmap_anonymous(
                        Some(extra_start),
                        extra_len,
                        ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                        MapFlags::MAP_ANONYMOUS
                            | MapFlags::MAP_PRIVATE
                            | MapFlags::MAP_FIXED_NOREPLACE,
                    );
                    litebox_util_log::debug!(
                        ok:% = extend_result.is_ok(),
                        result_addr:% = extend_result.as_ref().map(|p| p.as_usize()).unwrap_or(0);
                        "diag-tramp-extend: do_mmap_anonymous result"
                    );
                    if extend_result.is_err() {
                        litebox_util_log::warn!("failed to expand trampoline region");
                        self.apply_trap_fallback(mapped_addr, len, true);
                        restore_trampoline_rx(self, state);
                        return true;
                    }
                    state.trampoline_mapped_len = tramp_pages_needed;
                }

                // Write stubs before patching the code so rewritten jumps
                // never target an uninitialized trampoline.
                let tramp_write_ptr =
                    UserPtrMut::<u8>::from_usize(state.trampoline_addr + state.trampoline_cursor);
                if tramp_write_ptr
                    .copy_from_slice::<Platform>(0, &stubs)
                    .is_none()
                {
                    let _ = self.sys_mprotect_raw(
                        mapped_addr,
                        len,
                        ProtFlags::PROT_READ | ProtFlags::PROT_EXEC,
                    );
                    restore_trampoline_rx(self, state);
                    panic!("fatal: failed to write trampoline stubs");
                }

                // Write patched code back to the mapped region.
                if mapped_addr
                    .copy_from_slice::<Platform>(0, &code_buf)
                    .is_none()
                {
                    let _ = mapped_addr.copy_from_slice::<Platform>(0, &original_code);
                    let _ = self.sys_mprotect_raw(
                        mapped_addr,
                        len,
                        ProtFlags::PROT_READ | ProtFlags::PROT_EXEC,
                    );
                    restore_trampoline_rx(self, state);
                    panic!("fatal: failed to write patched code back to code segment");
                }
                state.trampoline_cursor = new_cursor;
                state.runtime_patches_committed = true;
            }
            Ok(_) => {
                // No trampoline stubs were generated, but the rewriter may
                // have replaced unpatchable syscalls with trap instructions.
                // Write back the modified code if it changed.
                if code_buf != original_code
                    && mapped_addr
                        .copy_from_slice::<Platform>(0, &code_buf)
                        .is_none()
                {
                    let _ = mapped_addr.copy_from_slice::<Platform>(0, &original_code);
                    panic!("fatal: failed to write trap bytes back to code segment");
                }
                // Fall through to restore RX protections below.
            }
            Err(e) => {
                litebox_util_log::warn!(err:? = e; "patch_code_segment failed");
                self.apply_trap_fallback(mapped_addr, len, true);
                restore_trampoline_rx(self, state);
                return true;
            }
        }

        // Restore the code segment to RX.
        let _ = self.sys_mprotect_raw(
            mapped_addr,
            len,
            ProtFlags::PROT_READ | ProtFlags::PROT_EXEC,
        );
        restore_trampoline_rx(self, state);
        true
    }

    /// The file-offset ranges of `fd`'s file that hold executable code, read from its ELF section
    /// headers and cached per `(device, inode)`.
    ///
    /// `None` means "could not tell" -- not an ELF64, no section header table, or an unreadable
    /// one. Callers must fall back to treating the whole mapping as code, never to skipping the
    /// rewrite: an unpatched `syscall` instruction escapes to the host kernel.
    fn executable_file_ranges(
        &self,
        fd: i32,
    ) -> Option<alloc::sync::Arc<alloc::vec::Vec<core::ops::Range<u64>>>> {
        let stat = self.sys_fstat(fd).ok()?;
        let key = (stat.st_dev, stat.st_ino);
        if let Some(cached) = self.global.exec_ranges_cache.lock().get(&key) {
            return Some(alloc::sync::Arc::clone(cached));
        }

        let mut header = [0u8; litebox_syscall_rewriter::ELF_HEADER_LEN];
        self.read_exact_at(fd, &mut header, 0)?;
        // What an ELF header means is the rewriter's business, and it already has `object`'s own
        // struct definitions -- decoding `e_shoff`/`e_shentsize`/`e_shnum` by hand here would be a
        // second, divergent copy of that knowledge expressed as byte offsets.
        let (e_shoff, e_shentsize, e_shnum) =
            litebox_syscall_rewriter::section_header_table_location(&header)?;
        let total = e_shentsize.checked_mul(e_shnum)?;
        // A sanity bound, so a corrupt header cannot ask for an unbounded allocation.
        if total > 16 * 1024 * 1024 {
            return None;
        }
        let mut section_headers = alloc::vec![0u8; total];
        self.read_exact_at(fd, &mut section_headers, usize::try_from(e_shoff).ok()?)?;

        let ranges = alloc::sync::Arc::new(
            litebox_syscall_rewriter::executable_section_file_ranges(
                &section_headers,
                e_shentsize,
                e_shnum,
            ),
        );
        if ranges.is_empty() {
            return None;
        }
        self.global
            .exec_ranges_cache
            .lock()
            .insert(key, alloc::sync::Arc::clone(&ranges));
        Some(ranges)
    }

    /// Fill `buf` from `offset` in `fd`, looping over short reads. `None` if it cannot be filled.
    fn read_exact_at(&self, fd: i32, buf: &mut [u8], offset: usize) -> Option<()> {
        let mut done = 0;
        while done < buf.len() {
            match self.sys_read(fd, &mut buf[done..], Some(offset + done)) {
                Ok(0) => return None,
                Ok(n) => done += n,
                Err(Errno::EINTR) => {}
                Err(_) => return None,
            }
        }
        Some(())
    }

    /// The [`SegmentScanCache`] key for the `len` bytes at `offset` in `fd`'s file.
    ///
    /// `None` when the descriptor has no stable `(dev, ino)` to key on, in which case the caller
    /// simply scans for itself -- see the call site in `maybe_patch_exec_segment`.
    fn segment_scan_key(&self, fd: i32, offset: usize, len: usize) -> Option<SegmentScanKey> {
        let stat = self.sys_fstat(fd).ok()?;
        Some((stat.st_dev, stat.st_ino, offset, len))
    }

    /// The [`ElfPatchCache`] key for `fd` in *this* task's process. See [`ElfPatchKey`].
    fn elf_patch_key(&self, fd: i32) -> ElfPatchKey {
        (self.pid.get(), fd)
    }

    /// Finalize the ELF patching state for `fd`.
    ///
    /// Removes the cache entry (preventing stale state if the fd is reused)
    /// and unmaps any trampoline that was allocated but never used.
    pub(crate) fn finalize_elf_patch(&self, fd: i32) {
        let state = self
            .global
            .elf_patch_cache
            .lock()
            .remove(&self.elf_patch_key(fd));
        if let Some(state) = state
            && state.trampoline_mapped
            && !state.pre_patched
            && !state.runtime_patches_committed
        {
            let tramp_len = state.trampoline_mapped_len;
            if tramp_len > 0 {
                let _ = self.sys_munmap(
                    UserPtrMut::<u8>::from_usize(state.trampoline_addr),
                    tramp_len,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use litebox::fs::{Mode, OFlags};
    #[cfg(target_os = "linux")]
    use litebox::platform::PageManagementProvider;
    #[cfg(target_os = "linux")]
    use litebox_common_linux::MRemapFlags;
    use litebox_common_linux::{MapFlags, ProtFlags, errno::Errno};

    use crate::syscalls::tests::TestPlatform as Platform;
    use crate::{UserPtrMut, syscalls::tests::init_platform};
    use litebox::mm::linux::PAGE_SIZE;

    #[test]
    fn test_anonymous_mmap() {
        let task = init_platform(None);

        let addr = task
            .sys_mmap(
                0,
                0x2000,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_ANON | MapFlags::MAP_PRIVATE,
                -1,
                0,
            )
            .unwrap();
        addr.write_slice_at_offset::<Platform>(0, &[0xff; 0x2000])
            .unwrap();
        assert_eq!(addr.read_at_offset::<Platform>(0x1000).unwrap(), 0xff,);
        task.sys_munmap(addr, 0x2000).unwrap();
    }

    #[test]
    fn test_file_backed_mmap() {
        let task = init_platform(None);

        let content = b"Hello, world!";
        let fd = task
            .sys_open("test.txt", OFlags::RDWR | OFlags::CREAT, Mode::RWXU)
            .unwrap();
        let fd = i32::try_from(fd).unwrap();
        assert_eq!(task.sys_write(fd, content, None).unwrap(), content.len());
        let addr = task
            .sys_mmap(
                0,
                0x1000,
                ProtFlags::PROT_READ,
                MapFlags::MAP_PRIVATE,
                fd,
                0,
            )
            .unwrap();
        assert_eq!(
            addr.to_owned_slice::<Platform>(content.len())
                .unwrap()
                .as_ref(),
            content.as_slice(),
        );
        task.sys_munmap(addr, 0x1000).unwrap();
        task.sys_close(fd).unwrap();
    }

    #[test]
    fn test_mremap() {
        let task = init_platform(None);

        // `old_size`/`new_size` must round up to genuinely DIFFERENT sizes
        // regardless of platform page size, or `sys_mremap`'s own rounding
        // (`old_size`/`new_size` each round up to a multiple of `PAGE_SIZE`)
        // collapses this into a same-size no-op that trivially succeeds
        // instead of exercising the in-place-growth-conflict path this test
        // means to check: a raw literal like `0x1000`/`0x2000` is only two
        // DIFFERENT page counts by coincidence on a 4 KiB-page platform --
        // both round up to the SAME single 16 KiB page on macOS.
        let old_size = PAGE_SIZE;
        let new_size = 2 * PAGE_SIZE;

        let addr = task
            .sys_mmap(
                0,
                new_size,
                ProtFlags::PROT_READ,
                MapFlags::MAP_ANON | MapFlags::MAP_PRIVATE,
                -1,
                0,
            )
            .unwrap();

        assert!(matches!(
            task.sys_mremap(
                addr,
                old_size,
                new_size,
                litebox_common_linux::MRemapFlags::empty(),
                0
            ),
            Err(litebox_common_linux::errno::Errno::ENOMEM)
        ),);
        let new_addr = task
            .sys_mremap(
                addr,
                old_size,
                new_size,
                litebox_common_linux::MRemapFlags::MREMAP_MAYMOVE,
                0,
            )
            .unwrap();
        task.sys_munmap(addr, new_size).unwrap();
        task.sys_munmap(new_addr, new_size).unwrap();
    }

    #[test]
    fn test_mmap_fixed_noreplace() {
        let task = init_platform(None);

        // Every offset below is expressed in units of `page` (never a raw
        // byte literal like `0x1000`): a literal that happens to be
        // page-aligned on a 4 KiB-page platform is not necessarily aligned to
        // macOS's 16 KiB pages, and every `MAP_FIXED_NOREPLACE` address below
        // must be exactly page-aligned or `sys_mmap` rejects it with EINVAL.
        let page = PAGE_SIZE;

        // First, create an initial mapping at a specific address away from
        // boundaries. No hardcoded literal is safe here on every platform: a
        // fixed-address `sys_mmap` IS checked against litebox's own tracked
        // mappings (which include a snapshot of the real host address space --
        // see `PageManagementProvider::reserved_pages`'s doc comment -- taken
        // at platform-construction time), but that snapshot can't account for
        // memory the host allocator claims dynamically afterward (e.g. while
        // this very test binary runs), so a literal picked to be free at
        // snapshot time can still collide for real by the time this test
        // actually runs. Get a genuinely free address instead: a hint-based
        // (non-fixed) mmap always avoids every existing mapping, host-reserved
        // or not.
        let probe_len = 4 * page;
        let base_addr = task
            .sys_mmap(
                0,
                probe_len,
                ProtFlags::PROT_READ,
                MapFlags::MAP_ANON | MapFlags::MAP_PRIVATE,
                -1,
                0,
            )
            .unwrap();
        task.sys_munmap(base_addr, probe_len).unwrap();
        // Leave a page of headroom below `base_addr` for the "adjacent
        // mapping right before" sub-test later, so it can't undo the
        // just-freed probe region's own neighbors.
        let base_addr = base_addr.as_usize() + page;

        let addr1 = task
            .sys_mmap(
                base_addr,
                2 * page,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_ANON | MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
            .unwrap();
        assert_eq!(
            addr1.as_usize(),
            base_addr,
            "First mapping should be at exact address"
        );

        // Test 1: Full overlap - should fail with EEXIST
        let err = task
            .sys_mmap(
                addr1.as_usize(),
                page,
                ProtFlags::PROT_READ,
                MapFlags::MAP_ANON | MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
            .unwrap_err();
        assert_eq!(err, Errno::EEXIST);

        // Test 2: Partial overlap at end - should fail with EEXIST
        // Existing: [addr1, addr1 + 2*page), New: [addr1 + page, addr1 + 3*page)
        let err = task
            .sys_mmap(
                addr1.as_usize() + page,
                2 * page,
                ProtFlags::PROT_READ,
                MapFlags::MAP_ANON | MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
            .unwrap_err();
        assert_eq!(err, Errno::EEXIST);

        // Test 3: Partial overlap at start - should fail with EEXIST
        // Existing: [addr1, addr1 + 2*page), New: [addr1 - page, addr1 + page)
        let err = task
            .sys_mmap(
                addr1.as_usize() - page,
                2 * page,
                ProtFlags::PROT_READ,
                MapFlags::MAP_ANON | MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
            .unwrap_err();
        assert_eq!(err, Errno::EEXIST);

        // Test 4: Adjacent mapping (right after) - should succeed
        let addr2 = task
            .sys_mmap(
                addr1.as_usize() + 2 * page,
                page,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_ANON | MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
            .unwrap();
        assert_eq!(addr2.as_usize(), addr1.as_usize() + 2 * page);

        // Test 5: Adjacent mapping (right before) - should succeed
        let addr3 = task
            .sys_mmap(
                addr1.as_usize() - page,
                page,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_ANON | MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
            .unwrap();
        assert_eq!(addr3.as_usize(), addr1.as_usize() - page);

        // Test 6: Zero address with MAP_FIXED_NOREPLACE - should fail with EPERM
        // (matches Linux behavior where vm.mmap_min_addr prevents mapping at address 0)
        let err = task
            .sys_mmap(
                0,
                page,
                ProtFlags::PROT_READ,
                MapFlags::MAP_ANON | MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
            .unwrap_err();
        assert_eq!(err, Errno::EPERM);

        // Clean up
        task.sys_munmap(addr3, page).unwrap();
        task.sys_munmap(addr1, 2 * page).unwrap();
        task.sys_munmap(addr2, page).unwrap();
    }

    // Windows-only historically, but no longer applicable there: `WindowsUserland::alloc` (the
    // host global allocator's `MemoryProvider` impl) now requests memory exclusively from
    // `HOST_ALLOCATOR_REGION_MIN..` (see that constant's doc comment in
    // `litebox_platform_windows_userland`), strictly above the guest's own `TASK_ADDR_MAX`, so
    // the collision this test manufactures (looping until the host allocator happens to land
    // inside guest-visible address space) is now structurally impossible on Windows -- the loop
    // below would spin until it exhausts the process's memory instead of terminating. Linux's
    // `MemoryProvider` impl has no equivalent partitioning, so the collision-avoidance behavior
    // under test remains real and exercised there.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_collision_with_global_allocator() {
        let task = init_platform(None);
        let platform = task.global.platform;
        let mut data = alloc::vec::Vec::new();
        // Find an address that is allocated to the global allocator but not in reserved regions.
        // LiteBox's page manager is not aware of the global allocator's allocations.
        let addr = loop {
            #[allow(
                unused_variables,
                reason = "the following features are mutually exclusive"
            )]
            #[cfg(target_os = "windows")]
            let addr = {
                let buf = alloc::vec::Vec::<u8>::with_capacity(0x10_0000);
                let addr = buf.as_ptr() as usize;
                data.push(buf);
                addr
            };
            #[cfg(target_os = "linux")]
            let addr = {
                let addr = unsafe {
                    libc::mmap(
                        core::ptr::null_mut(),
                        0x10_000,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    )
                } as usize;
                data.push(alloc::vec::Vec::<u8>::from(unsafe {
                    core::slice::from_raw_parts(addr as *const u8, 0x10_000)
                }));
                addr
            };

            let mut included = false;
            for r in <crate::syscalls::tests::TestPlatform as PageManagementProvider<
                4096,
            >>::reserved_pages(platform)
            {
                if r.contains(&addr) {
                    included = true;
                    break;
                }
            }

            if !included {
                // Also ensure that [addr - 0x1000, addr) is available, which is needed in the test below.
                if let Ok(ptr) = task.sys_mmap(
                    addr - 0x1000,
                    0x1000,
                    ProtFlags::PROT_READ,
                    MapFlags::MAP_PRIVATE | MapFlags::MAP_ANON,
                    -1,
                    0,
                ) {
                    if ptr.as_usize() != addr - 0x1000 {
                        task.sys_munmap(ptr, 0x1000).unwrap();
                        continue;
                    }
                    break addr;
                }
            }
        };

        // mmap with the found address should still succeed but not at the exact address.
        let res = task
            .sys_mmap(
                addr,
                0x1000,
                ProtFlags::PROT_READ,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_ANON,
                -1,
                0,
            )
            .unwrap();
        assert_ne!(res.as_usize(), 0);
        assert_ne!(res.as_usize(), addr);

        // grow the mapping without MREMAP_MAYMOVE should fail as the new region collides with the global allocator
        let err = task
            .sys_mremap(
                UserPtrMut::from_usize(addr - 0x1000),
                0x1000,
                0x2000,
                MRemapFlags::empty(),
                addr - 0x1000,
            )
            .unwrap_err();
        assert_eq!(err, Errno::ENOMEM);
    }

    /// `memfd_create` + `ftruncate` + `MAP_SHARED|PROT_WRITE` must produce REAL shared memory,
    /// not two independent copies: a write through one independent `mmap()` of the fd must be
    /// visible through a SECOND, separate `mmap()` of the same fd -- exactly the same "two
    /// independent mmaps observe each other's writes" proof this session's DRM dumb-buffer work
    /// established live against a real running process (see `docs/drm-dumb-buffer-ioctl-
    /// reference.md`'s history); this is the unit-test-level equivalent for `memfd_create`.
    #[test]
    fn test_memfd_create_shared_mapping_across_two_independent_mmaps() {
        let task = init_platform(None);

        let fd = task
            .sys_memfd_create(litebox_common_linux::MfdFlags::empty())
            .unwrap();
        let fd = i32::try_from(fd).unwrap();

        task.sys_ftruncate(fd, 0x1000).unwrap();

        let addr1 = task
            .sys_mmap(
                0,
                0x1000,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                fd,
                0,
            )
            .unwrap();
        let addr2 = task
            .sys_mmap(
                0,
                0x1000,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                fd,
                0,
            )
            .unwrap();
        assert_ne!(addr1.as_usize(), addr2.as_usize());

        addr1
            .write_slice_at_offset::<Platform>(0, &[0xab; 0x10])
            .unwrap();
        assert_eq!(addr2.read_at_offset::<Platform>(0).unwrap(), 0xab_u8);

        task.sys_munmap(addr1, 0x1000).unwrap();
        task.sys_munmap(addr2, 0x1000).unwrap();
        task.sys_close(fd).unwrap();
    }

    /// The REAL `wl_shm` client pattern this whole bridge exists for: `memfd_create`,
    /// `ftruncate`, write pixel bytes via an ORDINARY `write()` (not through any `mmap()` of its
    /// own), THEN a separate peer `mmap()`s the same fd -- it must see the bytes the writer put
    /// there. Live-witnessed end-to-end against the real `docs/wayland-drm-backend-probe`
    /// combined client+compositor probe (a genuine `wayland-client`/`smithay` pair, not a
    /// simulation): before this sync existed, the compositor's `mmap()`ed view read back all
    /// zero bytes despite the client's real `write()`s (`COMMIT_SHM_OK ... first4=[00, 00, 00,
    /// 00]`, confirmed live via a temporary diagnostic, immediately reverted); after, this exact
    /// unit-test shape passes.
    #[test]
    fn test_memfd_create_write_then_mmap_sees_the_written_bytes() {
        let task = init_platform(None);

        let fd = task
            .sys_memfd_create(litebox_common_linux::MfdFlags::empty())
            .unwrap();
        let fd = i32::try_from(fd).unwrap();

        task.sys_ftruncate(fd, 0x1000).unwrap();
        let content = [0xDD_u8, 0xCC, 0xBB, 0xAA].repeat(4);
        assert_eq!(
            task.sys_write(fd, &content, None).unwrap(),
            content.len()
        );

        let addr = task
            .sys_mmap(
                0,
                0x1000,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                fd,
                0,
            )
            .unwrap();
        assert_eq!(
            addr.to_owned_slice::<Platform>(content.len())
                .unwrap()
                .as_ref(),
            content.as_slice(),
        );

        task.sys_munmap(addr, 0x1000).unwrap();
        task.sys_close(fd).unwrap();
    }

    /// A fresh `memfd_create` fd (before any `ftruncate`) has no shared-memory object registered
    /// yet. `mmap(MAP_SHARED|PROT_WRITE)` on it must still produce a usable mapping rather than
    /// panicking or reading stale state -- which is also what real Linux does: a zero-length memfd
    /// can be mapped, and it is only an ACCESS past the end of the object that raises `SIGBUS`.
    ///
    /// This test previously asserted `ENODEV`, which was correct for the implementation that
    /// existed when it was written: `mmap(MAP_SHARED|PROT_WRITE)` on anything file-backed was
    /// refused outright. `try_shared_file_mmap` (see its doc comment -- `dconf`, and therefore
    /// every GSettings write in a MATE/GNOME/XFCE session, cannot survive that `ENODEV`)
    /// deliberately replaced that answer with a real shared object, and this expectation was never
    /// updated. It was not noticed because the whole test binary was crashing at test 9 of 181
    /// before reaching here -- see the VEH exception-code whitelist for that.
    #[test]
    fn test_memfd_create_mmap_before_ftruncate_is_usable() {
        let task = init_platform(None);

        let fd = task
            .sys_memfd_create(litebox_common_linux::MfdFlags::CLOEXEC)
            .unwrap();
        let fd = i32::try_from(fd).unwrap();

        let addr = task
            .sys_mmap(
                0,
                0x1000,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                fd,
                0,
            )
            .expect("mmap of a fresh memfd must produce a mapping, not ENODEV");

        // Zero-filled to start with, like any fresh anonymous memory, and actually writable --
        // "did not panic" alone would also be satisfied by a mapping that faults on first touch.
        assert_eq!(addr.read_at_offset::<Platform>(0).unwrap(), 0_u8);
        addr.write_slice_at_offset::<Platform>(0, &[0x5a; 0x10])
            .unwrap();
        assert_eq!(addr.read_at_offset::<Platform>(0).unwrap(), 0x5a_u8);

        task.sys_munmap(addr, PAGE_SIZE).unwrap();
        task.sys_close(fd).unwrap();
    }

    #[test]
    fn test_map_shared_anonymous() {
        let task = init_platform(None);

        // Two pages: `sys_mmap` itself always rounds a requested length up to
        // `PAGE_SIZE`, but every later `sys_mprotect`/`sys_munmap` call below
        // passes its length straight through, unrounded -- an arbitrary
        // literal like `0x2000` is only page-aligned by coincidence on a 4
        // KiB-page platform, and silently becomes an invalid, non-aligned
        // range on macOS's 16 KiB pages.
        let len = 2 * PAGE_SIZE;

        // MAP_SHARED | MAP_ANON with PROT_READ should succeed
        let addr = task
            .sys_mmap(
                0,
                len,
                ProtFlags::PROT_READ,
                MapFlags::MAP_ANON | MapFlags::MAP_SHARED,
                -1,
                0,
            )
            .unwrap();

        // Reading should work
        let _val: u8 = addr.read_at_offset::<Platform>(0).unwrap();

        // Anonymous shared mappings allow permission changes including write
        task.sys_mprotect(addr, len, ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
            .unwrap();
        addr.write_slice_at_offset::<Platform>(0, &[0xab; 0x10])
            .unwrap();
        assert_eq!(addr.read_at_offset::<Platform>(0).unwrap(), 0xab_u8);

        // mprotect to read-only should also succeed
        task.sys_mprotect(addr, len, ProtFlags::PROT_READ).unwrap();

        // ...but read-exec should NOT: Darwin's W^X enforcement permanently
        // refuses to add PROT_EXEC to any mapping that was ever writable (see
        // docs/macos.md's "W^X, MAP_JIT, and code signing" section) -- this
        // mapping was made PROT_WRITE above, so on macOS this transition is
        // rejected rather than silently degraded, matching the same
        // "unimplemented rather than silently wrong" posture as this
        // platform's other W^X-affected paths.
        #[cfg(not(target_vendor = "apple"))]
        task.sys_mprotect(addr, len, ProtFlags::PROT_READ_EXEC)
            .unwrap();
        #[cfg(target_vendor = "apple")]
        assert_eq!(
            task.sys_mprotect(addr, len, ProtFlags::PROT_READ_EXEC)
                .unwrap_err(),
            Errno::EACCES,
        );

        task.sys_munmap(addr, len).unwrap();
    }

    #[test]
    fn test_map_shared_anonymous_writable() {
        let task = init_platform(None);

        // MAP_SHARED | MAP_ANON with PROT_WRITE should succeed
        let addr = task
            .sys_mmap(
                0,
                0x1000,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_ANON | MapFlags::MAP_SHARED,
                -1,
                0,
            )
            .unwrap();

        addr.write_slice_at_offset::<Platform>(0, &[0xcd; 0x10])
            .unwrap();
        assert_eq!(addr.read_at_offset::<Platform>(0).unwrap(), 0xcd_u8);

        task.sys_munmap(addr, 0x1000).unwrap();
    }

    /// Real cross-process aliasing for `MAP_SHARED` anonymous memory: a write through the
    /// "child"'s (duplicated `PageManager`'s) mapping must be visible through the original
    /// "parent" mapping, proving the two are backed by the SAME underlying platform
    /// shared-memory object rather than independent copies -- the actual guarantee
    /// `mmap-map-shared-real-cross-process-semantics` exists to provide. `PageManager::duplicate`
    /// is exercised directly (the same primitive `do_clone` uses for real `fork()`) rather than
    /// through a full `clone()`/thread-spawn, since the cross-process visibility guarantee lives
    /// entirely in `Vmem::duplicate`'s shared-mapping branch, not in thread/register plumbing.
    #[test]
    fn test_map_shared_survives_fork_duplicate() {
        let task = init_platform(None);

        let addr = task
            .sys_mmap(
                0,
                0x1000,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_ANON | MapFlags::MAP_SHARED,
                -1,
                0,
            )
            .unwrap();
        addr.write_slice_at_offset::<Platform>(0, &[0x11; 0x10])
            .unwrap();

        // Simulate the address-space duplication `fork()` performs.
        let (_child_pm, relocations) =
            unsafe { task.process().pm().duplicate(&task.global.litebox) }.unwrap();
        let child_addr: UserPtrMut<u8> =
            UserPtrMut::from_usize(relocations.translate(addr.as_usize()).unwrap());

        // The child's mapping starts with the parent's contents (proving it's the SAME memory,
        // not merely identically-initialized independent memory would already be a weaker,
        // insufficient guarantee here since the eager-copy path also does that).
        assert_eq!(child_addr.read_at_offset::<Platform>(0).unwrap(), 0x11_u8);

        // A write through the CHILD's mapping must be visible through the PARENT's mapping --
        // this is the actual cross-process sharing guarantee, and cannot be explained by
        // identical-initial-contents alone.
        child_addr
            .write_slice_at_offset::<Platform>(0, &[0x22; 0x10])
            .unwrap();
        assert_eq!(addr.read_at_offset::<Platform>(0).unwrap(), 0x22_u8);

        // And the reverse direction: a write through the PARENT's mapping must be visible
        // through the CHILD's.
        addr.write_slice_at_offset::<Platform>(0, &[0x33; 0x10])
            .unwrap();
        assert_eq!(child_addr.read_at_offset::<Platform>(0).unwrap(), 0x33_u8);

        task.sys_munmap(addr, 0x1000).unwrap();
    }

    #[test]
    fn test_map_shared_readonly_file() {
        let task = init_platform(None);

        let content = b"Hello, shared!";
        let fd = task
            .sys_open("shared.txt", OFlags::RDWR | OFlags::CREAT, Mode::RWXU)
            .unwrap();
        let fd = i32::try_from(fd).unwrap();
        assert_eq!(task.sys_write(fd, content, None).unwrap(), content.len());

        // `sys_mmap` itself always rounds a requested length up to `PAGE_SIZE`,
        // but the later `sys_mprotect`/`sys_munmap` calls below pass their
        // length straight through, unrounded -- `0x1000` is only page-aligned
        // by coincidence on a 4 KiB-page platform, and silently becomes an
        // invalid, non-aligned range on macOS's 16 KiB pages.
        let len = PAGE_SIZE;

        // MAP_SHARED with PROT_READ on a file should succeed
        let addr = task
            .sys_mmap(0, len, ProtFlags::PROT_READ, MapFlags::MAP_SHARED, fd, 0)
            .unwrap();

        // Data should match
        assert_eq!(
            addr.to_owned_slice::<Platform>(content.len())
                .unwrap()
                .as_ref(),
            content.as_slice(),
        );

        // `mprotect` adding write permission SUCCEEDS here, and that is correct: the fd above was
        // opened `O_RDWR`, and real Linux only answers `EACCES` when the file was opened without
        // write permission. This asserted `EACCES` for as long as `MAP_SHARED` mapped the file
        // read-only at the host level, which is no longer how it works (see
        // `try_shared_file_mmap`); the expectation outlived the implementation it described.
        task.sys_mprotect(addr, len, ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
            .expect("mprotect may add write to a MAP_SHARED mapping of an O_RDWR fd");
        // And the promotion is real, not merely accepted.
        addr.write_slice_at_offset::<Platform>(0, b"W").unwrap();
        assert_eq!(addr.read_at_offset::<Platform>(0).unwrap(), b'W');

        task.sys_munmap(addr, len).unwrap();
        task.sys_close(fd).unwrap();
    }

    /// `mmap(MAP_SHARED|PROT_WRITE)` on a file-backed fd gives every mapper of that file the SAME
    /// memory -- the guarantee `try_shared_file_mmap` exists to provide, and the one `dconf` builds
    /// its staleness flag out of (a writer maps the byte `PROT_WRITE`, readers map it `PROT_READ`
    /// and poll it; see that function's doc comment for why refusing this made `mate-panel` come up
    /// with no panels at all).
    ///
    /// Two separate histories meet in this test. It was originally written because the combination
    /// used to `todo!()` and crash the whole runner on an idiom as ordinary as Python's
    /// `mmap.mmap(fd, length, mmap.MAP_SHARED, mmap.PROT_WRITE)`; the fix then was to refuse it
    /// with `ENODEV`, which is what this asserted. `try_shared_file_mmap` later replaced that
    /// refusal with real shared memory and left the assertion behind. Neither the staleness nor the
    /// failure was visible, because the test binary was crashing before this test ran -- see the
    /// VEH exception-code whitelist.
    #[test]
    fn test_map_shared_writable_file_is_shared_between_mappers() {
        let task = init_platform(None);
        let fd = task
            .sys_open(
                "shared_writable.txt",
                OFlags::RDWR | OFlags::CREAT,
                Mode::RWXU,
            )
            .unwrap();
        let fd = i32::try_from(fd).unwrap();
        assert_eq!(task.sys_write(fd, b"seed", None).unwrap(), 4);

        let writer = task
            .sys_mmap(
                0,
                PAGE_SIZE,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                fd,
                0,
            )
            .expect("MAP_SHARED|PROT_WRITE on a file must map, not fail with ENODEV");

        // Seeded from the file's current bytes by the first mapper.
        assert_eq!(writer.read_at_offset::<Platform>(0).unwrap(), b's');

        // A SECOND, independent read-only mapping of the same file must be the same memory, not a
        // private snapshot -- this is the whole point, and an identically-seeded private copy would
        // pass a content check while failing this one.
        let reader = task
            .sys_mmap(
                0,
                PAGE_SIZE,
                ProtFlags::PROT_READ,
                MapFlags::MAP_SHARED,
                fd,
                0,
            )
            .expect("a second MAP_SHARED mapping of the same file must succeed");
        writer
            .write_slice_at_offset::<Platform>(0, b"MADE")
            .unwrap();
        assert_eq!(reader.read_at_offset::<Platform>(0).unwrap(), b'M');

        // The documented limitation, asserted rather than left to drift: writes through the
        // mapping do NOT reach the file's byte storage, so a `read()` still sees the pre-`mmap`
        // contents. If that ever changes, this is the test that should be updated to say so.
        let mut via_read = [0u8; 4];
        assert_eq!(task.sys_read(fd, &mut via_read, Some(0)).unwrap(), 4);
        assert_eq!(&via_read, b"seed");

        task.sys_munmap(reader, PAGE_SIZE).unwrap();
        task.sys_munmap(writer, PAGE_SIZE).unwrap();
        task.sys_close(fd).unwrap();
    }

    /// Contrast case for [`test_map_shared_writable_file_returns_enodev_instead_of_panicking`]
    /// just above: a file created under `/dev/shm` is real Linux's own tmpfs, so unlike an
    /// ordinary file it must NOT hit that `ENODEV` rejection -- `MAP_SHARED|PROT_WRITE` there is
    /// exactly the real shared-memory semantics glibc's `shm_open` relies on (see
    /// `syscalls::file::is_dev_shm_path`'s doc comment for the open-time `MemfdMarker` tagging
    /// this exercises, and the runner's `initialize_root_in_mem_layer` for why `/dev/shm` exists
    /// as a directory at all). Live-verified against a real freestanding guest probe
    /// (`advisor/probes/shm_probe.c`) doing the identical `open+ftruncate+mmap+write+read-back`
    /// sequence before this unit test was written -- this is the regression-test-level
    /// equivalent.
    #[test]
    fn test_dev_shm_file_supports_map_shared_write() {
        let task = init_platform(None);
        // `/dev` already exists in this test's own fixture tar (`litebox/src/fs/test.tar`) --
        // unlike the real runner's fresh in-mem layer, which needs it created explicitly (see
        // `initialize_root_in_mem_layer`'s doc comment) -- so only `/dev/shm` needs creating here.
        let _ = task.sys_mkdirat(
            litebox_common_linux::AT_FDCWD,
            "/dev",
            (Mode::RWXU | Mode::RGRP | Mode::ROTH).bits(),
        );
        task.sys_mkdirat(
            litebox_common_linux::AT_FDCWD,
            "/dev/shm",
            (Mode::RWXU | Mode::RWXG | Mode::RWXO).bits(),
        )
        .unwrap();

        let fd = task
            .sys_open(
                "/dev/shm/probe_name",
                OFlags::RDWR | OFlags::CREAT | OFlags::EXCL,
                Mode::RUSR | Mode::WUSR,
            )
            .unwrap();
        let fd = i32::try_from(fd).unwrap();

        task.sys_ftruncate(fd, 0x1000).unwrap();

        let addr = task
            .sys_mmap(
                0,
                0x1000,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                fd,
                0,
            )
            .unwrap();
        addr.write_slice_at_offset::<Platform>(0, &[0xab; 0x10])
            .unwrap();
        assert_eq!(addr.read_at_offset::<Platform>(0).unwrap(), 0xab_u8);

        task.sys_munmap(addr, 0x1000).unwrap();
        task.sys_close(fd).unwrap();
    }

    #[test]
    fn test_madvise() {
        let task = init_platform(None);

        let addr = task
            .sys_mmap(
                0,
                0x2000,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_ANON | MapFlags::MAP_PRIVATE,
                -1,
                0,
            )
            .unwrap();

        addr.write_slice_at_offset::<Platform>(0, &[0xff; 0x10])
            .unwrap();

        // Test MADV_NORMAL
        assert!(
            task.sys_madvise(addr, 0x2000, litebox_common_linux::MadviseBehavior::Normal)
                .is_ok()
        );

        // Test MADV_DONTNEED
        assert!(
            task.sys_madvise(
                addr,
                0x2000,
                litebox_common_linux::MadviseBehavior::DontNeed
            )
            .is_ok()
        );

        addr.to_owned_slice::<Platform>(0x10)
            .unwrap()
            .iter()
            .for_each(|&x| {
                assert_eq!(x, 0); // Should be zeroed after MADV_DONTNEED
            });

        task.sys_munmap(addr, 0x2000).unwrap();
    }

    /// Regression test: every `MadviseBehavior` variant beyond `Normal`/`DontFork`/`DoFork`/
    /// `DontNeed`/`Free` used to unconditionally panic (`unimplemented!("Unsupported madvise
    /// behavior")`), crashing the whole runner -- reachable from something as ordinary as
    /// Python's `mmap.madvise(mmap.MADV_WILLNEED)` or musl/glibc allocators issuing
    /// `MADV_HUGEPAGE`-style hints. Advisory-only hints (real Linux accepts every one of these
    /// as a no-op success regardless of whether the kernel backs the hint with real behavior)
    /// must return `Ok`; `MADV_REMOVE`/`MADV_HWPOISON`/`MADV_SOFT_OFFLINE` must fail cleanly
    /// with `EINVAL` rather than panicking.
    #[test]
    fn madvise_covers_every_behavior_without_panicking() {
        use litebox_common_linux::MadviseBehavior;

        let task = init_platform(None);
        let addr = task
            .sys_mmap(
                0,
                0x1000,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_ANON | MapFlags::MAP_PRIVATE,
                -1,
                0,
            )
            .unwrap();

        for advice in [
            MadviseBehavior::Random,
            MadviseBehavior::Sequential,
            MadviseBehavior::WillNeed,
            MadviseBehavior::Mergeable,
            MadviseBehavior::Unmergeable,
            MadviseBehavior::HugePage,
            MadviseBehavior::NoHugePage,
            MadviseBehavior::DontDump,
            MadviseBehavior::DoDump,
            MadviseBehavior::WipeOnFork,
            MadviseBehavior::KeepOnFork,
            MadviseBehavior::Cold,
            MadviseBehavior::Pageout,
            MadviseBehavior::PopulateRead,
            MadviseBehavior::PopulateWrite,
            MadviseBehavior::DontNeedLocked,
        ] {
            assert!(
                task.sys_madvise(addr, 0x1000, advice).is_ok(),
                "advisory-only madvise behavior must succeed as a no-op, not panic"
            );
        }

        for advice in [
            MadviseBehavior::Remove,
            MadviseBehavior::HWPoison,
            MadviseBehavior::SoftOffline,
        ] {
            assert_eq!(
                task.sys_madvise(addr, 0x1000, advice).unwrap_err(),
                Errno::EINVAL,
                "unsupported-mapping madvise behavior must fail cleanly, not panic"
            );
        }

        task.sys_munmap(addr, 0x1000).unwrap();
    }

    // Signal support for Windows is not ready yet.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn test_fallible_read() {
        let _ = init_platform(None);

        let ptr = UserPtrMut::<u8>::from_usize(0xdeadbeef);
        let result = ptr.read_at_offset::<Platform>(0);
        assert!(result.is_none());
    }
}
