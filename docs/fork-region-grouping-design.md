# Permanent fix design: provenance-based region grouping in `Vmem::duplicate`

Status: **design only, not implemented.** Written 2026-09-06 after the live confirmation
described below. The current shipped state is a *diagnostic probe* (commit `e78987f6`), not
this design.

## The bug this replaces

`litebox/src/mm/linux.rs`, `Vmem::duplicate` (the `fork()` path). litebox cannot give a forked
child the same virtual addresses real Linux `fork()` gives it, so it relocates the parent's
regions into freshly-chosen destination addresses. Relocating each region *independently* breaks
any pointer arithmetic that crosses a region boundary -- RIP-relative `.text` -> `.got`
references, and allocator-internal arithmetic between a heap chunk and its metadata.

The existing mitigation partitions regions into **groups** and relocates each group as one
contiguous span, preserving every pairwise offset *within* a group exactly. Regions in different
groups may land anywhere relative to each other.

Grouping is decided by a single **address-distance heuristic**:

```rust
let max_intra_group_gap: usize = 16 * 1024 * 1024;  // pre-e78987f6
// ...
Some(last) if r.start <= last.end.saturating_add(max_intra_group_gap) => { /* merge */ }
_ => groups.push(r),
```

16MiB was chosen by reasoning about **musl mallocng's** group/meta_area spacing (see the long
comment above the constant). That is the defect: the constant encodes one specific allocator's
layout, and nothing checks that the guest actually uses that allocator.

### Why it broke on Debian/glibc

glibc's allocator lays memory out differently: each per-thread arena reserves **64MiB** of
address space, and the main arena grows via `brk` while large allocations are individually
`mmap`ped. Inter-arena gaps therefore exceed 16MiB *by design* -- 4x the threshold from the arena
size alone. On a glibc guest, `fork()` split arenas into separately-placed groups and broke
exactly the cross-region arithmetic the grouping exists to preserve.

The symptom is not an obvious crash at the corrupting write. It is **heap metadata that reads
back wrong in the child**, caught later and elsewhere by glibc's own allocator integrity checks:

- `malloc(): corrupted top size`
- `malloc(): unaligned fastbin chunk detected`

This explains every confusing property of the bug: corruption is present long before detection;
whichever process touches a broken cross-group pointer first is the one that visibly dies (so the
first-to-die process varies run to run and means nothing); it only appears on fork-heavy
workloads; and it was never seen on the Alpine/busybox tests, which are musl -- the allocator
16MiB was actually tuned for.

### Live evidence (2026-09-06)

`linuxserver/webtop:debian-xfce`, `/usr/bin/Xorg :0`, fresh release build, `max_intra_group_gap`
raised to 512MiB. Two clean boots, **zero** corruption signatures where both prior reproductions
aborted during startup:

- **run B**: 30 X extensions initialized (GLX, DRI3, Present, RANDR, COMPOSITE, DAMAGE, ...),
  idle at the normal `AIGLX: Screen 0 is not DRI2 capable` steady state.
- **run C**: same 30 extensions, plus real modesetting on litebox's virtual DRM device --
  `Virtual-1 connected`, `virtual-1920x1080` modeline, KMS color map for depth 24, DPMS enabled,
  `Setting screen physical size to 508 x 285`.

## Why 512MiB is not the fix

1. **It is still a distance heuristic.** It happens to clear glibc's arena spacing on this
   workload. A guest whose allocator spaces regions more widely -- or a long-running process whose
   heap fragments across a wider span -- regresses silently, with the same
   corrupt-now-crash-later signature that cost this project multiple full investigations.
2. **It over-merges.** The 16MiB value was explicitly reasoned about as keeping the guest stack in
   its own group. At 512MiB the stack can merge into the heap/ELF group. That is harmless for
   *correctness* (grouping only ever preserves relative offsets; merging never invalidates a
   pointer) but it inflates each reservation and abandons a property the original author
   deliberately relied on.
3. **It encodes allocator internals in the VM layer.** `Vmem::duplicate` should not need to know
   what allocator the guest runs.

The two failure directions are asymmetric, and that asymmetry is the whole design pressure:

| | too small | too large |
|---|---|---|
| effect | groups split that must stay together | groups merged that need not be |
| cost | **silent heap/GOT corruption**, detected much later, in an arbitrary process | larger contiguous reservations, possible allocation failure under address-space pressure |

Under-merging is a correctness bug. Over-merging is a resource cost. **When uncertain, merge.**

## The fix: group by mapping provenance, not by address distance

Two regions must share a group **iff** guest code may legitimately compute a pointer from one
into the other. That is a property of *how the regions were created*, which litebox already knows
at creation time and currently throws away before `fork()`.

### Provenance classes

Tag each `VmArea` at creation with the identity of the guest operation that produced it:

- `ElfObject(id)` -- a `PT_LOAD` segment of one loaded ELF image. All segments of one image share
  an `id`. This is the RIP-relative `.text` -> `.got` case, and is **exactly** correct: the linker
  computed one coherent layout, so every segment of one object must keep its offsets and no
  segment of a *different* object needs to.
- `Brk` -- the main-arena heap. One group per process.
- `MmapCluster(id)` -- an anonymous `mmap` region. Its arithmetic relationships are the hard case;
  see below.
- `Stack(tid)` -- a thread stack. Never needs a relationship to anything else.
- `Shared` / `GuardPage` -- already relocated independently today; unchanged.

`ElfObject`, `Brk`, `Stack` are precise, cheap, and remove the guess entirely for the majority of
regions. The residual difficulty is confined to anonymous `mmap`.

### Handling anonymous mmap (where allocators live)

An allocator's arithmetic between a chunk and its out-of-line metadata is exactly the case the
16MiB constant was trying to cover, and provenance alone cannot prove two independent `mmap`
calls are related. Options, in preference order:

1. **Coalesce by allocator-visible adjacency.** Merge anonymous regions into one group when the
   guest itself treats them as one span -- contiguous or separated only by `PROT_NONE` guard pages
   that the *same* mapping call reserved. This captures arena-with-guard-region layouts (glibc's
   64MiB arena reservation is one `mmap` with most of it `PROT_NONE`, committed incrementally)
   without any distance constant. This alone likely covers the glibc arena case exactly, because
   an arena *is* one reservation.
2. **One group for all anonymous private regions.** Simplest and always correct (merging is never
   unsafe). Cost is one large reservation spanning the guest's whole anonymous range. Worth
   measuring before dismissing -- on 64-bit, address space is not the scarce resource, and this
   removes the entire class of bug permanently.
3. **Keep a distance fallback for anonymous regions only**, with the constant justified as a
   resource-vs-safety tradeoff rather than as an allocator fact, and documented as the known-unsafe
   edge.

Recommendation: implement (1), fall back to (2) where provenance is unavailable. Never (3) alone.

### The real fix, if it is ever reachable

All of this exists only because the child cannot get the parent's addresses. If litebox can ever
reserve the child's address space at the *same* virtual addresses (the guarantee real `fork()` has
for free), grouping becomes unnecessary and the entire class disappears. The blocker is that the
host Windows allocator owns placement. Worth a scoped feasibility pass on
`MEM_ADDRESS_REQUIREMENTS`-directed reservation before investing heavily in grouping heuristics --
`GroupRelocation`'s existing rounding logic already fights this same constraint.

## Verification standard for any change here

This bug class is invisible at the moment it is introduced, so "it booted" is not evidence.

- Reproduce on **glibc** (`debian-xfce`) *and* **musl** (Alpine) -- the two allocators have
  opposite failure modes and a fix for one can regress the other. musl was the original 16MiB
  motivation; do not lose it.
- Exercise a genuinely **fork-heavy** workload, not a single process. Xorg alone idling proves
  much less than Xorg with real clients.
- Grep the guest's own allocator integrity output (`malloc():`, `corrupted`, `fastbin`,
  `SIGABRT`), not just process exit status -- glibc detects and aborts, so the abort message is
  the signal and an exit code is not.
- Repeat boots. The first-to-die process varies run to run; a single clean run is weak evidence.
