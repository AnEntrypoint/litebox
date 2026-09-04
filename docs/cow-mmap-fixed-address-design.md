# Design: making Windows CoW-mmap work for `MAP_FIXED` ELF loads

## Status

Design only. Not implemented. Written per an explicit user instruction to scope this
carefully as a separate pass from implementation, given this exact area's demonstrated
fragility (see "Prior fragility" below).

## Problem, precisely

`try_allocate_cow_pages` (`litebox_platform_windows_userland/src/lib.rs`) is the Windows
implementation of `PageManagementProvider::try_allocate_cow_pages`. When it succeeds, a guest
`mmap(MAP_PRIVATE, fd, offset)` of a static, tar-backed file gets a real copy-on-write view
instead of paying a page-by-page `sys_read` + memcpy loop (`do_mmap_file_memcpy` in
`litebox_shim_linux/src/syscalls/mm.rs`).

It currently **always fails** (falls back to memcpy) for every real ELF `PT_LOAD` segment
load, because:

- Windows' `MapViewOfFile3` requires its `Offset` parameter (the offset into the backing
  file) to be a multiple of the allocation granularity, `0x1_0000` (64KiB).
- Real ELF `PT_LOAD` segment file offsets are only ever page-aligned (4KiB), fixed by the
  linker. A packer cannot change this without re-linking or splicing padding into the
  binary's own physical layout, both out of scope (confirmed today, AGENTS.md passes
  321/342/347/349/"CoW alignment settlement": three real layers at 1%/90%/100% *tar-entry*
  alignment produced byte-identical CoW-attempt/CoW-success counts, because tar-entry
  alignment only controls where a *file* starts in the tar, never where a *segment* starts
  *within* that file).
- `litebox_shim_linux::loader::elf::map_file` (the only caller that ever loads a `PT_LOAD`
  segment) always issues `MAP_FIXED`. There is no `Hint`-mode ELF-segment load anywhere in
  this codebase. Confirmed today by direct code reading, not inference.
- For a `FixedAddressBehavior::Replace`/`NoReplace` (`MAP_FIXED`/`MAP_FIXED_NOREPLACE`)
  request, `try_cow_mmap_file` (`litebox_shim_linux/src/syscalls/mm.rs`) requires the
  returned pointer to equal the requested `suggested_start` **exactly**:

  ```rust
  if fixed_behavior == FixedAddressBehavior::Replace
      && let Some(requested) = suggested_addr
      && ptr.as_usize() != requested
  {
      return Some(Err(MappingError::OutOfMemory));
  }
  ```

  A mismatch is not tolerated or silently retried; it fails the whole `mmap`.

So: the *file*-offset alignment requirement and the *address*-exactness requirement
together mean a correct fix must place a view whose *content* starts at a 4KiB-aligned file
offset, at a *host address* that exactly equals `suggested_start`, using a Windows API whose
*view offset* is constrained to 64KiB. The natural move — start the view 64KiB-aligned
earlier in the file, and place its host base `view_padding` bytes before `suggested_start`
— requires those `view_padding` guest-address bytes to be genuinely available. They usually
aren't:

## Why the address space "isn't free" — read from the actual reservation code

`litebox_shim_linux/src/loader/elf.rs::ElfFile::reserve` is called **once per ELF, up
front**, for `len = max_vaddr - min_vaddr` (the whole ELF's virtual span, computed by the
parser from every `PT_LOAD`'s `p_vaddr`/`p_memsz`). It does:

1. `sys_mmap` a `PROT_NONE | MAP_ANONYMOUS | MAP_PRIVATE` region of
   `mapping_len = len + (align.max(PAGE_SIZE) - PAGE_SIZE)` bytes — deliberately larger than
   `len` so there's room to find an `align`-aligned sub-window inside it.
2. `compute_reserved_regions` (`litebox_common_linux/src/loader.rs`) computes exactly where
   that aligned sub-window is (`aligned_ptr = mapping_ptr.next_multiple_of(align)`) and
   **immediately `munmap`s everything outside it** (`head_unmap`, `tail_unmap`).
3. The final reservation is `[aligned_ptr, aligned_ptr + len)` — **page-tight**, with no
   slack anywhere, not at the start, not at the end, and (since it's one contiguous mmap)
   not between segments either.

Every individual `PT_LOAD` segment is then `MAP_FIXED`-mapped (`map_file`) into its own
sub-range of this single, already page-tight reservation, later, one at a time, as the
loader processes the ELF's program headers.

**Consequence for CoW's padding trick:** for any `PT_LOAD` segment after the first, the
`view_padding` bytes immediately preceding its `suggested_start` are, in the overwhelming
common case, **inside this same ELF's own single up-front reservation** — either a
neighboring segment's own mapped/about-to-be-mapped span, or `PROT_NONE` inter-segment
padding that `Vmem` already considers claimed (reserved, if not yet backed) by this exact
process. A raw "ask Windows for the address, see if it's free" collision probe cannot
distinguish "genuinely free host address space" from "already claimed by this process's own
`Vmem` bookkeeping, just not yet backed by content." `MapViewOfFile3`/`VirtualAlloc2` would
likely succeed at that address anyway (Windows doesn't know litebox's own reservation
semantics), silently overwriting/aliasing memory `Vmem` believes is reserved for something
else.

## Prior fragility in exactly this spot (do not repeat)

Pass 343 attempted a version of this trick for the `Hint` case only (deliberately avoiding
`Replace`/`NoReplace` for the reason above). It mapped the padded, 64KiB-aligned-at-the-file
view via `MapViewOfFile3` with an *unconstrained* host base address (`base_addr = null`),
then returned a pointer `view_padding` bytes into that view. This shipped, built, and passed
its own unit tests.

Pass 344 found and reverted a real, live memory-safety bug in it: the `view_padding` bytes
*before* the returned pointer are real, physically-mapped, guest-*inaccessible*-but-
host-*present* memory that `try_allocate_cow_pages` never reported to its caller. `Vmem`
only ever learned about `len = source_data.len()` bytes starting at the *returned* pointer —
not the padding. Consequences, reproduced live (`/usr/bin/labwc --help` against
`webtop_seatd.tar`, `view_padding=61440`, fault at `cr2=0xa0f2a0`, inside the padding):

- A guest access into the padding faulted, and litebox's own SIGSEGV path reported it as
  "genuinely unmapped" (no `Vmem` record), even though the OS had it mapped.
- Worse: this codebase's crash-cleanup path then called `VirtualFree(MEM_DECOMMIT)` on that
  same untracked range, which fails outright (`os error 487`) because the memory is backed
  by a `MapViewOfFile3` view, not a `VirtualAlloc` region — `VirtualFree(MEM_DECOMMIT)` is
  never valid there, only `UnmapViewOfFileEx` is, and there was no path to that call for the
  padding either.

The lesson this design must not repeat: **any host-mapped byte that isn't registered with
`Vmem` is a live bug waiting for a guest access or a teardown path to find it.** This applies
equally to a `Replace`/`NoReplace` version of the trick, and is the central constraint the
rest of this document designs around.

## Options considered

### Option A — rely on implicit slack in `ElfFile::reserve`'s existing reservation

**Ruled out by direct code reading, not by inference.** As shown above,
`compute_reserved_regions` trims the reservation to be page-tight around `[aligned_ptr,
aligned_ptr + len)` with `head_unmap`/`tail_unmap` explicitly removing anything outside that
exact window. There is no slack at the start (trimmed to `aligned_ptr`), none at the end
(trimmed to `aligned_ptr + len`), and none between segments (the whole span between
`min_vaddr` and `max_vaddr` is one contiguous reservation with nothing removed from the
middle). This option does not exist as stated; it requires *becoming* Option B or C.

### Option B — make the reservation cover the CoW padding too, and track it explicitly

Extend `ElfFile::reserve`'s up-front reservation to include up to `ALLOCATION_GRANULARITY -
PAGE_SIZE` (60KiB) of *extra* slack **before** each `PT_LOAD` segment's `p_vaddr`, sized to
exactly cover whatever `view_padding` that segment's own CoW attempt will need — and have
`try_allocate_cow_pages` explicitly register that padding as its own tracked, `PROT_NONE`,
guest-inaccessible mapping (via the existing, general-purpose
`litebox::mm::Vmem::register_existing_mapping` — already used by this exact call site,
already supports arbitrary `MemoryRegionPermissions` including `empty()` for PROT_NONE, no
new tracking primitive needed) rather than leaving it unregistered.

Concretely:

1. **Compute `view_padding` before reservation, not just before mapping.** Currently
   `ElfFile::reserve` reserves purely from the ELF's own `p_vaddr` layout, with no knowledge
   of file offsets or CoW eligibility at all — that's a *different* function's problem today
   (`try_allocate_cow_pages`, called much later, per-segment, from `try_cow_mmap_file`). To
   make the reservation padding-aware, the loader needs to know each segment's *file*
   offset's own misalignment (`p_offset % ALLOCATION_GRANULARITY`) at reservation time —
   this data is already available from the parsed ELF headers before `reserve()` runs
   (`ElfParsedFile`'s program header table), it just isn't threaded through today.
2. **Reserve `p_vaddr - view_padding` instead of `p_vaddr`, per segment, where geometrically
   possible.** This is a real structural change to `ElfFile::reserve`/`compute_reserved_regions`:
   they currently compute ONE aligned window for the WHOLE ELF span in one shot, with no
   per-segment reasoning. Making the reservation account for per-segment padding needs
   either (i) widening the single whole-ELF reservation on the *low* end by the *first*
   segment's own padding need (cheap, but only helps the first segment — later segments'
   padding still falls inside the single contiguous reservation, already covered, since nothing
   is trimmed between segments — see below) or (ii) reasoning about each segment's `p_vaddr`
   individually. Re-reading the trim logic with this specific question in mind: **since the
   whole-ELF reservation is contiguous with no inter-segment trimming**, the address range
   immediately before any *non-first* segment's `p_vaddr` is *already inside* the same
   reservation (it's simply unbacked `PROT_NONE` slack belonging to whatever the ELF's layout
   put there — either genuinely between segments per the ELF's own layout, or another
   segment's own content). This means Option B's real, minimal-diff form only needs to widen
   the *start* of the whole-span reservation (to cover the *first* `PT_LOAD` segment's own
   possible padding need) — every later segment's padding is already inside the existing
   reservation and just needs to be correctly *registered* (step 3), not additionally
   *reserved*.
3. **Register the padding as PROT_NONE in `Vmem` at the moment `try_allocate_cow_pages`
   creates the padded view**, via the same `register_existing_mapping` call
   `try_cow_mmap_file` already makes for the real content — either as one call covering
   `[view_base, view_base + view_padding + len)` with the padding portion given
   `MemoryRegionPermissions::empty()` and the real content portion given the requested
   `permissions` (would need `register_existing_mapping` or a sibling to support a
   split-permission range, which it does not today — check `VmArea`/`Vmem`'s insert path for
   whether two adjacent single-permission `register_existing_mapping` calls, one for the
   padding and one for the content, compose correctly instead — this is very likely simpler
   and requires no new API), or as two separate calls (padding: `empty()`; content: the real
   `permissions`) using the API as it exists today. **Prefer two separate calls** — no new
   `Vmem`/`VmArea` API surface needed, and it mirrors how `ElfFile::reserve` itself already
   treats "the reserved-but-not-yet-backed slack" as ordinary `PROT_NONE` memory tracked the
   same way as everything else.
4. **Teardown**: once correctly registered as an ordinary `PROT_NONE` mapping, the padding
   needs no special teardown logic at all — it's just another VMA that the existing generic
   munmap/process-exit paths already handle (this was exactly the bug in pass 343/344: the
   padding *wasn't* an ordinary tracked VMA, so ordinary teardown paths didn't know about it
   and reached for the wrong Windows API). This is the main safety payoff of Option B over a
   pass-343-style ad hoc trick: **no new teardown code path, because there's no untracked
   memory left to need one.**

**Risk**: real, but bounded and specific. `ElfFile::reserve`/`compute_reserved_regions` are
on the hot path of *every* guest exec (not just CoW-eligible ones) — a mistake here has much
broader blast radius than a bug confined to `try_allocate_cow_pages` alone. The change is
conceptually simple (widen a size computation, thread one more input through) but touches
code shared by 100% of execs. `compute_reserved_regions` is a pure function
(`litebox_common_linux/src/loader.rs`) with no direct unit tests visible in a first pass over
the file (needs confirming) — any change here needs new, dedicated unit tests for the
padding-aware case *and* confirmation the existing non-padded case is provably unaffected
when `view_padding == 0` (should reduce to exactly today's behavior).

**Estimated cost**: moderate. Touches `ElfFile::reserve`, `compute_reserved_regions`
(`litebox_common_linux`), and `try_allocate_cow_pages` (`litebox_platform_windows_userland`).
Needs new plumbing to get file-offset-alignment info from the ELF parser into `reserve()`
(today only `len`/`align` — a memory-layout concern — reach it; file-offset-per-segment is a
loading concern currently kept separate). Real, non-trivial design work beyond this document
if pursued (see "If proceeding" below).

### Option C — a Windows API without the 64KiB file-offset constraint

Researched via the Windows API documentation family this codebase already depends on
(`windows-sys`, imports already present in `litebox_platform_windows_userland/src/lib.rs`).

- `MapViewOfFile3`/`MapViewOfFile3FromApp` (currently used): explicitly documented
  (Microsoft Learn, `Memory/nf-memoryapi-mapviewoffile3`) to require the `Offset` parameter
  be a multiple of the system allocation granularity, not just the page size. This is the
  same constraint every `MapViewOfFile*` variant has had since Windows NT — it derives from
  how the underlying section-view mechanism aligns to allocation granularity for TLB/working-
  set-list reasons, not an arbitrary API restriction that a different function bypasses.
- `NtMapViewOfSection`/`NtMapViewOfSectionEx` (undocumented-but-widely-used native API,
  `ntdll.dll`): does **not** relax this constraint either — every public description and
  reverse-engineering reference of `NtMapViewOfSection`'s `SectionOffset` parameter states
  the same allocation-granularity requirement, because it's the same underlying kernel
  section-mapping mechanism `MapViewOfFile3` is a wrapper over, not a different one. There is
  no lower-level escape hatch here; verified before writing rather than assumed.
- No other file-view-mapping primitive exists in the public or native Windows API surface
  that maps a *sub-range* of a file section at an arbitrary (non-granularity-aligned) file
  offset. Every path ultimately goes through the same section-object/view mechanism with the
  same alignment rule.

**Conclusion: Option C does not exist.** This is a genuine, structural Windows platform
constraint, not a gap in this codebase's own API usage. Any fix must work around it at the
address-space-management level (Option B), not bypass it with a different API call.

### Option D — don't fix it

Genuinely worth weighing given:

- **The expected benefit is real but not yet demonstrated to dominate this workload's actual
  cost.** CoW eliminates real, deterministic work (`~386` `sys_read` calls per busybox exec
  that would otherwise memcpy real file bytes — confirmed today, syscall-count level,
  identical across three tar-alignment levels). But this session's own noise-floor
  measurement (AGENTS.md, "10 reps, one fixed config") found real host-level wall-clock
  variance of ~20% peak-to-peak on this specific workload shape, and the tar-alignment
  investigation's headline "26.6ms→~0ms/exec" bare-shell number did **not** hold up under a
  live GUI session (measured 3x *worse*, mechanism still not conclusively explained as of
  this design's writing) — i.e., **this exact area of the codebase has produced multiple
  wall-clock numbers today that did not survive proper replication or a real workload
  re-test.** Before investing in Option B's real implementation risk, whoever picks this up
  should have a clean, replicated (not n=1) measurement of what CoW-succeeding actually saves
  in wall-clock terms for the real target workload (a live GUI session doing repeated execs),
  not just a syscall-count argument. The syscall-count elimination is real and deterministic;
  its wall-clock value on THIS host, for THIS workload, is not yet established with the same
  rigor.
- **This exact code path (fork_verify/CoW/ELF-segment-loading interaction) has produced
  three distinct real bugs in one day** (pass 343→344's CoW padding memory-safety bug, described
  above; the still-unresolved, 30+-pass archived `fork_verify` investigation with an already-
  retracted root-cause theory, `docs/AGENTS_ARCHIVE_2026-09-03.md`; and this session's own
  explicit project constraint, "do not attempt a fourth fix attempt without new diagnostic
  evidence," recorded against a related but distinct `fork_verify` bug). This is a real,
  demonstrated pattern of fragility specifically in memory-management code that interacts
  with ELF loading and Windows fork emulation, not a generic "be careful" caveat.
- **Counter-argument for proceeding anyway**: Option B, as scoped above, does NOT touch
  `fork_verify` at all — it's confined to `ElfFile::reserve`/`compute_reserved_regions`
  (loading-time, one-shot, well-isolated) and `try_allocate_cow_pages`
  (platform-mmap-time, already well-isolated behind a trait boundary with an established
  safe-fallback contract). It is a materially different, narrower risk surface than the
  fork_verify bugs above. The main real risk is the shared-by-every-exec blast radius of
  touching `ElfFile::reserve` at all, not an inherent connection to the already-fragile
  modules.

**Recommendation: proceed with Option B, but only after first getting a clean, replicated
wall-clock (or, better, a cycle-accurate/syscall-timed) measurement of CoW's actual value for
the live-GUI workload** — i.e., do the measurement work first, separately, before investing
in Option B's real implementation and test-writing cost. If that measurement shows the
benefit is real and worth the implementation risk, Option B is the correct, safe design (no
untracked memory, no new teardown path, reuses existing `Vmem` primitives, bounded and
well-understood risk). If the measurement shows the benefit is marginal or unmeasurable on
this host, Option D (leave the safe `Unaligned` fallback in place indefinitely) is the
correct, honest call — the memcpy fallback path is correct today, just not optimal, and
"not optimal but correct" beats "a fourth memory-safety bug in this exact code today."

## If proceeding with Option B: concrete implementation plan

1. **Measurement first** (see above) — a clean, ≥5-rep, real-GUI-session, real-exec-workload
   before/after (memcpy path vs. a *hand-simulated* CoW win, e.g. by temporarily hacking
   `do_mmap_file_memcpy` to skip the actual `sys_read` loop and just report success, purely
   to measure the theoretical upper bound without yet building the real fix) to confirm the
   win is worth the implementation cost. Do this as its own small, throwaway experiment, not
   as part of the real implementation.
2. **Thread file-offset-alignment awareness into `ElfFile::reserve`.** Add a parameter (or a
   pre-computed value derived from the already-parsed `ElfParsedFile`'s program headers) so
   `reserve()` knows the *first* `PT_LOAD` segment's `p_offset % ALLOCATION_GRANULARITY`
   before computing `mapping_len`/calling `sys_mmap`. Widen `mapping_len`'s low end by that
   amount (bounded by `ALLOCATION_GRANULARITY - PAGE_SIZE` = 60KiB max) so the final
   `aligned_ptr` has genuine, `Vmem`-reserved slack immediately before it, sized to exactly
   what CoW might need for the first segment.
3. **Verify (don't assume) that non-first segments' padding is already covered.** Confirm via
   a focused reading of `compute_reserved_regions` and the loader's per-segment `map_file`
   call sequence that the reservation between segment N's end and segment N+1's start is
   genuinely still `PROT_NONE`-reserved-but-unbacked at the time segment N+1's CoW attempt
   runs (not, e.g., already partially punched-through by an earlier segment's own
   `map_zero`/BSS handling) — write a small standalone test/probe that dumps the actual VMA
   layout mid-load to confirm this empirically before relying on it.
4. **In `try_allocate_cow_pages`**: when `view_padding != 0` (any `FixedAddressBehavior`, not
   just `Hint`), place the view at `suggested_start - view_padding` (constrained via the
   existing `MEM_ADDRESS_REQUIREMENTS` machinery already used elsewhere in this function —
   reuse, don't reinvent) instead of unconditionally falling back to `Unaligned`. On success,
   register the padding prefix as its own `PROT_NONE` mapping via
   `register_existing_mapping` **before** returning the content pointer to the caller (so
   there is never a window where the padding is host-mapped but not yet `Vmem`-tracked) —
   check whether this needs to happen inside `try_allocate_cow_pages` itself (which does not
   currently call `register_existing_mapping` — that's the caller's job, `try_cow_mmap_file`)
   or whether the trait/call boundary needs to change so `try_allocate_cow_pages` can report
   "I also mapped this extra padding range, please register it" back to its caller. Given the
   `Vmem` registration API lives in `litebox` core and `try_allocate_cow_pages` lives in the
   platform crate (which does not have direct `Vmem` access — confirm this layering by
   checking the trait's own module boundaries), the cleanest shape is likely: extend
   `CowAllocationError`'s `Ok` return type (or add a sibling out-parameter) to report
   `Option<(padding_start, padding_len)>` back to `try_cow_mmap_file`, which already has
   `Vmem` access and already calls `register_existing_mapping` for the content range — have
   it make a second call for the padding range too, right next to the existing one.
5. **Only apply this for segments after reservation has confirmed the padding is safe**
   (step 3) — if the geometric verification in step 3 shows a specific segment's preceding
   range is NOT safely `Vmem`-reserved-and-unbacked (e.g. edge cases around the very first
   segment if step 2's widening didn't cover a particular offset, or any layout this design
   didn't anticipate), fall back to the existing, safe `Unaligned` path for that segment —
   never attempt the address-shift trick without the geometric guarantee from step 2/3 in
   place. This preserves "no untracked memory" as an absolute invariant, not a best-effort
   one.
6. **Tests**: unit tests for `compute_reserved_regions`'s new padding-aware behavior
   (including `view_padding == 0` reducing to today's exact behavior — a required regression
   guard), a new `litebox_shim_linux` test mirroring the shape of
   `test_dev_shm_file_supports_map_shared_write`/`test_map_shared_writable_file_returns_enodev_instead_of_panicking`
   (search for these two existing tests as templates — same file, same "contrast pair" style)
   for "CoW with a misaligned-but-now-padded-and-registered file offset succeeds and the
   padding prefix is genuinely inaccessible to the guest" (i.e. write a test that a guest
   access to the padding range correctly faults as PROT_NONE, not as "unmapped" — the exact
   distinction pass 343/344's bug got wrong). A live-boot verification (busybox/mate-session/a
   full XFCE session) confirming zero regression, matching this session's own established
   discipline of live-verifying every platform-level change today (passes 342-349 all did
   this).
7. **Re-measure** the real workload (not the throwaway hack from step 1) with the actual
   implementation, replicated (≥5 reps per condition, per this session's own noise-floor
   lesson), before claiming any wall-clock win.

## Summary

- Option A (implicit slack): does not exist, ruled out by reading `compute_reserved_regions`.
- Option C (different Windows API): does not exist, ruled out by verifying against the
  actual Windows section-mapping API family.
- Option B (extend the reservation, track the padding as an ordinary `PROT_NONE` VMA) is the
  only viable path, is well-scoped, reuses existing `Vmem` primitives with no new tracking
  concept needed, and directly avoids pass 343/344's specific failure mode (untracked mapped
  memory) by construction rather than by care.
- Recommend getting a clean, replicated measurement of the real benefit *before* investing in
  Option B's implementation cost, given this exact workload's demonstrated measurement
  fragility today. If the benefit doesn't hold up, Option D (leave the current, safe
  `Unaligned` fallback as the permanent behavior) is a legitimate, honest outcome — not a
  failure to find a fix, but a correct call that the fix isn't worth its risk for an unproven
  gain.
