# ADVISORY-002: Getting `D == 0` fork on Windows — feasibility, and the one blocker that actually matters

Status: research and design. No code changed in this pass.

Companion to `ADVISORY-001-fundamentals.md` sections 1.2, 3N and 5. Section 3N established
*why* the relocating in-process fork cannot be healed (glibc safe-linking is XOR-keyed by the
slot's own address, so relocation by a delta `D` destroys it irrecoverably). This advisory
answers the follow-on question: **what would it actually take to get `D == 0`, and is
`RtlCloneUserProcess` the way to do it?**

The headline is not what the question assumed.

---

## 0. BLUF

1. **`D == 0` is not a future project. It already exists in-tree, is already wired into the
   real `fork()` path, and already works** — `LITEBOX_PROCESS_FORK=1`, passes 108-157,
   `spawn_cross_process_fork_child`. It gives the child a genuinely separate Windows process
   whose memory sits at the **exact source addresses** (`copy_one_group` deliberately ignores
   `dest_base`), and hands that child an identity relocation map.
2. **It is hard-gated off for every real workload** by one line:
   `fd_complexity.beyond_stdio == 0` (`litebox_shim_linux/src/syscalls/process.rs:3167`).
   Any guest holding a single fd at or above 3 — i.e. every XFCE component, every X client,
   every D-Bus participant — silently falls back to the broken thread-based relocating fork.
3. **The blocker is therefore not the address space. It is the fd table**, and behind it the
   rest of the kernel-equivalent state, which is pointer-rich Rust living in the parent's
   private heap and unreachable from another process.
4. `RtlCloneUserProcess` is **proven working on this host** (`advisor/probes/clone_probe.c`,
   Windows 11 10.0.26200) and is a strictly better copy mechanism than today's
   `VirtualAlloc2` + `WriteProcessMemory`. But swapping it in **does not by itself unlock
   anything**, because the fd/kernel-state problem is orthogonal to how the memory got copied.
5. The narrow "special-case tcache/PTR_MANGLE" fallback (option B) is **unbounded**, and one
   of its sub-problems (`heap_for_ptr`) has no pointer to fix at all. It should not be taken.
6. **This is a fork-*without*-exec problem specifically, and that leaves a narrower path open.**
   `fork()` + `execve()` has been safe all along — the child tears down and reloads before any
   stale pointer is dereferenced — and a program the runner execs directly as pid 1 is never
   forked at all, so it is trivially safe. Confirmed live (see §1.5). So `D == 0` is the correct
   general fix and is still needed for any real multi-process guest, but it is **not** a hard
   prerequisite for demonstrating a working desktop endpoint.
7. **Recommendation: finish the path that is already 80% built.** The ordered blockers are
   (i) presenter-process split, (ii) cross-process `RawMutex`, (iii) fixed-base shared kernel
   heap, (iv) fd/HANDLE indirection. Only then does relaxing the `beyond_stdio` gate become
   meaningful — and at that point `RtlCloneUserProcess` is a cheap, high-value swap for the
   copy leg.

---

## 1. The finding that reframes the question

`ADVISORY-001` 3N recommends, as the next step, "a standalone probe that clones the runner
with `RtlCloneUserProcess` and runs guest code in the child." That step is, in substance,
already done — by a different mechanism, and further along than a probe.

`litebox_shim_linux/src/syscalls/process.rs:3167-3186`:

```rust
if fd_complexity.beyond_stdio == 0
    && let Some(mut full_gprs) = cross_process_gprs
    && { full_gprs.fs_base = cross_process_fs_base; true }
    && let Some(handle) = self.global.platform
        .spawn_cross_process_fork_child(&relocations, full_gprs)
{
    self.process().register_cross_process_child(child_tid, handle);
    return Ok(usize::try_from(child_tid).unwrap());
}
```

The comment above it calls this "production process-based fork()". On success the guest child
"is ALREADY running for real in a genuinely separate Windows process … exactly like real
fork(): both parent and child continue independently."

And it is genuinely `D == 0`. `litebox_platform_windows_userland/src/process_fork.rs:2926`:

```rust
fn copy_one_group(
    child: HANDLE,
    source_group: &Range<usize>,
    _dest_base: usize,          // <-- deliberately unused
    ...
```

Step 1 of that function force-reserves "the group's exact span, at its exact SOURCE address,
in the child" via `VirtualAlloc2` with `MEM_ADDRESS_REQUIREMENTS`, which per its own comment
"either lands EXACTLY here or the call fails outright". The parent then hands the child
`relocations.identity_for_cross_process()` (`litebox/src/mm/mod.rs:197`), mapping every range
to itself.

So the child's addresses equal the parent's. Safe-linked tcache pointers, PTR_MANGLE'd
pointers, `heap_for_ptr` masks, DTV entries — all of 3N's failure class — are correct in that
child by construction, with nobody having to understand any of them.

### Why this has not already fixed XFCE

`beyond_stdio` is `raw_descriptors.iter_alive().filter(|&raw| raw >= 3).count()`
(`process.rs:2644`). The gate's own justification (`process.rs:3150-3153`) is honest about the
reason:

> none of this shim's 7 fd subsystems are backed by a real Windows HANDLE, so a cross-process
> child cannot inherit anything beyond the 0/1/2 stdio slots the OS itself hands a fresh
> `CreateProcessW` child automatically

That is the whole problem in one sentence. litebox's fds are not OS handles; they are
`Arc<RwLock<DescriptorEntry>>` in a `Vec<Option<IndividualEntry>>`
(`litebox/src/fd/mod.rs:26-28`, `894-897`), living in the host heap. A separate process cannot
see them.

### Consequence for how the historical evidence should be read

`docs/AGENTS_ARCHIVE_2026-09-03.md` (94th pass) records that `LITEBOX_PROCESS_FORK=1`
reproduced a crash "IDENTICALLY" to thread-based fork, and concludes the bug is
"implementation-independent". That conclusion is **not** evidence against `D == 0`. Two
reasons: the archive predates 3N's symbolization and was chasing a different, `execve`-time
allocator bug (which the same archive later attributes to "the first buffered-I/O allocation
performed by ANY process that reached its current state via `execve()`"); and any repro
holding a socket or pipe never entered the cross-process path at all — it fell through the
`beyond_stdio` gate into the ordinary relocating fork.

**Before any large investment, this is the cheap decisive test**: run a fork repro that keeps
`beyond_stdio == 0` (stdio only, no sockets, no pipes) under `LITEBOX_PROCESS_FORK=1`, against
a glibc guest, and confirm the `__libc_malloc+0x76` fault does not occur. That is a direct
experimental check of 3N's central claim, on existing code, with no design change.

### 1.5 The blast radius is narrower than it looked: only fork-WITHOUT-exec dies

Measured live (2026-09-06), and it sharpens the whole picture:

- **Running as pid 1 is safe, trivially and confirmed.** The runner `exec`s its top-level
  program directly — there is no fork anywhere in that path. Run with
  `-- /usr/bin/Xorg :0 ...` (no shell, no `&`), Xorg runs **completely cleanly**: zero malloc
  aborts, zero SIGABRT/SIGSEGV, stable 5+ minutes. The full display pipeline comes up — wgpu
  presenter (adapter, device, queue, Mailbox present mode), DRM framebuffer at exactly
  1920×1080×4, and `LITEBOX_DUMP_FRAMES` writing real frames to disk.
- **fork + exec is also safe, confirmed in the same run.** That run forked twice internally
  (`/bin/sh`, `/usr/bin/xkbcomp`); **both exited status=0**.
- Every crash in this investigation has been in a forked child that did **not** exec.

This is exactly what the mechanism predicts. `fork_verify` arms `EFLAGS.TF` "from the moment a
`fork()` child resumes until it reaches `execve`/`exit`/`exit_group` (after which the parent's
addresses are unreachable and the danger is over)" (`fork_verify.rs:47`). A child that execs
promptly discards the whole inherited heap — including every safe-linked tcache freelist —
before allocating again. A child that *doesn't* exec keeps using it, and hits
`__libc_malloc+0x76` on the first tcache pop.

Why the earlier evidence pointed the other way: both prior test harnesses backgrounded Xorg
with `&` inside a shell, and that `&` is itself a bash fork. Xorg had never actually been run
as pid 1.

**Consequence for planning.** The remaining problem is not "the fork mechanism is broken and
needs an architectural rewrite before anything can be demonstrated." It is the much narrower
"get clients onto an already-working, already-stable X server without going through a
fork-without-exec." One promising shape, under test: run each client as its own runner
instance's pid 1, as siblings against the same shared display, avoiding shell forking
altogether.

Stated honestly in both directions:

- The captured frames are solid black (`non_black_pixels=0`) — correct and expected for a bare
  X server with no clients connected, and **not** a claim that the desktop endpoint is reached.
- `D == 0` remains the correct, general, permanent fix. Any real multi-process guest workload
  eventually needs genuine `fork()` semantics: `dbus-daemon --fork`, xfsettingsd and
  Thunar `--daemon` all fork *without* exec by design, and no sibling-runner arrangement makes
  those work.
- But the two are **decoupled**. A working demo may well be reachable tonight, on existing
  code, while the architectural fix proceeds on its own timeline.

---

## 2. `RtlCloneUserProcess`: what it buys, and what it does not

### Proven on this host

`advisor/probes/clone_probe.c` (Windows 11 10.0.26200, 2026-09-03) establishes, in one pass:
clone succeeds and the child resumes at the same instruction with `STATUS_PROCESS_CLONED`;
private memory is copy-on-write isolated; a `SEC_RESERVE` view mapped **before** the clone
stays shared **and** a page the parent commits **after** the clone is visible to the child;
inherited pipe handles work; both `NtCreateThreadEx` and kernel32 `CreateThread` work in the
child; the exit code is delivered. `advisor/probes/apc_probe.c` additionally proves
`QueueUserAPC2` with `QUEUE_USER_APC_FLAGS_SPECIAL_USER_APC` interrupts a cloned child's
thread spinning in pure user mode — the cross-process signal primitive.

One negative result, and it is load-bearing: **keyed events do not rendezvous across
processes.** Both sides time out.

### What it improves over the current mechanism

The current `CreateProcess` + `VirtualAlloc2` + `WriteProcessMemory` path achieves `D == 0`,
but pays for it:

| | today (`CreateProcess` + copy) | `RtlCloneUserProcess` |
|---|---|---|
| addresses | identical, but only by *forcing* each group's base | identical for free (kernel CoW) |
| 64 KiB granularity | hard constraint; forces the whole reservation-*group* concept (`linux.rs:1319-1389`) and can fail on collision with the child's own fresh image/DLLs | no constraint; whole space duplicated |
| copy cost | eager, full `WriteProcessMemory` of every group | lazy, copy-on-write |
| `MAP_SHARED` | `Vmem::duplicate` **fails outright** on any `VM_SHARED` mapping (`mm/mod.rs:770`) | section views survive the clone (probe-proven) |
| handles | only stdio; everything else lost | whole table replicated, **same handle values** |
| threads | child image freshly loaded, state rebuilt | exact replica of the calling thread |

The `MAP_SHARED` and handle rows are the significant ones. Today a fork by any guest holding a
shared mapping fails; under clone it works. And "same handle values in the child" is exactly
what the fd-table problem needs, *for the subset of fds that ever become real handles*.

### What it does not buy

It does not make `Arc<RwLock<DescriptorEntry>>` visible across processes, because a CoW clone
gives the child a **private** copy of the parent's heap. Two processes then diverge silently:
the parent's `write()` to a pipe never reaches the child's copy of the pipe buffer. So clone
alone converts a "child cannot see the fd table" bug into a "child sees a frozen snapshot of
the fd table" bug, which is worse — silent instead of gated.

**This is the crux, and it is why clone is not the unlock.** Shared kernel state has to be in
a section deliberately mapped shared, at a fixed base, *before* the clone. The probe already
proves that shape works (`SEC_RESERVE` view mapped pre-clone stays shared, and post-clone
parent commits are visible). But the state has to be moved there first.

### Risk notes

- **CSRSS.** A clone child must not load DLLs or touch console/user32/GDI/COM. litebox
  currently violates this: `winit`/`wgpu` run on a thread in the guest-hosting process
  (`presentation.rs`, deps at `litebox_platform_windows_userland/Cargo.toml:59-60`), and there
  are ~24 `Win32::System::Console` references including `SetConsoleCtrlHandler`
  (`lib.rs:2627`) and a `ConsoleStdinReader` thread (`lib.rs:7034`). `docs/presenter-process-design.md`
  is the fix and says so itself — but it is explicitly "design only, no code in this pass."
- **EDR.** Cloning APIs are monitored because `PssCaptureSnapshot`/`NtCreateProcessEx` against
  LSASS is a credential-dumping pattern (Elastic ships prebuilt rules). A non-LSASS self-clone
  is much less likely to trip them, but the API is a watched surface. Worth knowing before
  shipping to third-party machines; not a development-time concern.
- **Not deprecated**, still functional on 26200 by direct measurement. midipix ships a
  CoW fork on NT-native cloning, so this is a proven approach for real POSIX workloads.
  (Cygwin, by contrast, does *not* clone: it `CreateProcess`es and rebuilds the address space,
  relying on identical DLL bases, with a retry loop and `rebaseall` — closer to litebox's
  current mechanism than to clone.)

---

## 3. The three real blockers, in dependency order

### 3.1 Presenter split (hard prerequisite, already designed, unbuilt)

`docs/presenter-process-design.md` and `ADVISORY-001` Appendix D specify it fully. Its one
open risk was already retired by `advisor/probes/dup_probe.c` (cross-process `DuplicateHandle`
of a scanout section, no admin rights). Console I/O must move to file/pipe handles in the same
pass. Nothing else can proceed safely until the guest-hosting process is CSRSS-clean.

### 3.2 Cross-process `RawMutex` (well-contained; the design is forced)

**Every native address- or TID-based wait on Windows is process-local by design.** Not a
limitation to route around — a documented property of all four candidates:

- `WaitOnAddress`/`WakeByAddress` — MSDN says "another thread **in the same process**".
- `NtWaitForKeyedEvent`/`NtReleaseKeyedEvent` — the released thread "must be within the same
  process as the signaling"; confirmed by `clone_probe.c` (both sides time out).
- `NtAlertThreadByThreadId`/`NtWaitForAlertByThreadId` — takes a bare TID, but returns
  `STATUS_ACCESS_DENIED` cross-process. These are what SRWLock/condvar/`WaitOnAddress` are
  built on since Win8, which is why the first bullet holds.
- Named mutex/semaphore — works, but a kernel object and a syscall per operation.

So the only cross-process wake is **through a shared kernel object**. The design is therefore
forced, and matches `ADVISORY-001` 1.2 item 3: a shared state word in the shared section for
the uncontended fast path, plus a per-waiter auto-reset `Event` whose handle is duplicated
into the waker's process on demand and cached per `(process, waiter)`.

**The good news is containment.** `WaitOnAddress`/`WakeByAddressSingle` appear in **exactly
one place in the entire repository**: `litebox_platform_windows_userland/src/lib.rs:5170` and
`:5269`, inside a single ~150-line `RawMutex`. The trait it implements
(`litebox/src/platform/mod.rs:247-296`) is already precisely futex semantics —
`underlying_atomic()`, `wake_many(n)`, `block(val)`, `block_or_timeout(val, t)` — and every
shim subsystem (pipes, sockets, ptys, futexes) reaches synchronization only through
`RawSyncPrimitivesProvider`, which bottoms out there. **One impl to replace, not a diffuse
refactor.**

### 3.3 Fixed-base shared kernel heap (mandatory, because the state is pointer-rich)

All shared kernel state is owned host-heap Rust — `Arc`, `Box`, `BTreeMap`, `Vec` — never POD.
So it can only be shared by mapping one pagefile-backed section at **the same fixed base in
every process**, and allocating that state out of it.

The seam exists and is the right one:
`#[global_allocator] static SLAB_ALLOC: SafeZoneAllocator<'static, 34, WindowsUserland>`
(`lib.rs:7635`).

The state itself is unusually concentrated — a build-time ratchet (`dev_tests/src/ratchet.rs:98`)
forbids new bare statics, so it lives in two heap singletons instead:

- `LiteBoxX { platform, descriptors }` (`litebox/src/litebox.rs:112`) — the fd table.
- `GlobalState`, 22 fields (`litebox_shim_linux/src/lib.rs:2243-2347`) — futex manager, pipes,
  network, pid/tid allocator, AF_UNIX address table, flock registry, pty registries, memfd
  registry, DRM, evdev, and the id counters. Several fields' own doc comments already say they
  must be shared across forked processes (`flock_registry` at 2269, `pty_registry` at 2283).

Plus, outside `GlobalState`: `DefaultFS` (mount/VFS state), the `shared_pending` signal queue,
and the per-process fd tables.

**Two constraints worth flagging that the existing design notes do not:**

- **Trait-object vtables.** `DescriptorEntry` holds `Box<dyn FdEnabledSubsystemEntry>`
  (`litebox/src/fd/mod.rs:915`). A vtable pointer points into the runner image's `.rodata`, so
  it is only valid in another process if the runner is loaded at the **same base**. A clone
  inherits the base for free; a freshly-spawned process does not, and would need
  `/DYNAMICBASE:NO` or equivalent. Under clone this is a non-issue — another reason clone beats
  the `CreateProcess` mechanism.
- **Reserve size and placement.** Neither an official maximum reserved-section size nor a
  guaranteed collision-free high-VA band is documented; place high in the 64-bit space and
  verify at runtime rather than assuming.

### 3.4 HANDLE indirection and cross-guest memory access

Objects embedding raw `HANDLE`s (memfds, ptys, sockets, thread handles, `ADV_SHM_VIEWS`) must
carry an object id and resolve lazily via `DuplicateHandle` from the owning process. Clones
share the token, so this is permitted.

One cost the existing design notes do not enumerate: **cross-guest memory access.** Guest
addresses are host addresses today — `UserMutPtr` is `#[repr(transparent)]` over a `usize`
with `with_exposed_provenance_mut` — so a kernel write into another task's memory is a plain
store. That is fine for a guest touching its own memory (and clone preserves it exactly), but
`process_vm_readv`, the `clear_child_tid` write on thread death, `wait4` status writes, and
signal-frame construction all write into *another* process's memory in the new model. Each
needs `NtReadVirtualMemory`/`NtWriteVirtualMemory` or staging through the shared section.

---

## 4. Why the narrow fallback (option B) should not be taken

The proposal was: special-case exactly the known glibc mechanisms rather than solving the
general problem. Assessed honestly, it fails on four independent grounds.

**Reachability.** `tcache` is `static __thread`, so it has internal linkage and is **absent
from `libc.so.6`'s `.dynsym` entirely** — not findable by `nm -D`. `main_arena` is likewise a
non-exported `static`, and the classic locator (fixed offset from `__malloc_hook`) died when
glibc 2.34 removed the hooks. A stripped libc in a container layer — the normal case — has
neither symbol.

**Layout instability.** The formula is stable; the *struct* is not. `tcache_entry` gained a
`key` field in 2.30; `counts` widened `char`→`uint16_t` in 2.30; and **glibc 2.42 renamed
`counts` to `num_slots`, inverted its meaning, and grew it 64→76** for large-bin tcache.
Anything hardcoding 64 silently mis-indexes.

**PTR_MANGLE is genuinely the easier half — and that does not help.** Confirmed: x86-64
mangling is `xor %fs:0x30` then `rol $17`; the guard is `tcbhead_t.pointer_guard`, seeded once
per exec from `AT_RANDOM` and copied verbatim to every thread. It is **not** keyed by the
pointer's own address, so the child inherits the same guard and a demangle yields the
*parent's* address — wrong by `D` but exactly recoverable via demangle → `+D` → re-mangle.
That is a bijection with a known constant, and materially more tractable than safe-linking.
But it is in the same fork as safe-linking, which is not tractable, so fixing it leaves the
crash.

**One sub-problem has no pointer to fix at all.** glibc's `heap_for_ptr` is
`PTR_ALIGN_DOWN(ptr, HEAP_MAX_SIZE)` — it recovers the `heap_info` by **masking a chunk's own
address** on every non-main-arena `free()`. There is nothing to walk and nothing to rewrite. It
is correct only if `D` is a multiple of `HEAP_MAX_SIZE` (~64 MiB). Failure is silent, then
catastrophic. The same shape recurs in mimalloc (32 MiB segment mask, plus its own per-page
encoded pointers), CPython's `POOL_ADDR`, and V8's 4 GiB cage. jemalloc/tcmalloc need full
radix-tree *rekeying*, not sweeping.

**Scope, honestly:** not a line count but a matrix — (glibc version × build × arch) layouts,
a TLS offset for a symbol not in `.dynsym`, an arena locator that no longer exists, per-thread
FS bases, and the same again per third-party allocator. Realistically thousands of lines plus
a per-build offset database, and still not correct.

**This road has already been walked four times in this repo.** `fixup_stale_stack_pointers`
went 64 KiB scan → thousands of false heals → 4 KiB → still 38/40 corrupt → root-caused to a
*command-length-dependent* ash `stalloc` false positive (clean at lengths 6-12 and 29+, 100%
corrupt at 13-28) → fixed only by adding an executable-range filter that knowingly narrowed
coverage. `fork_verify.rs`'s `MIN_POINTER_ALIGN` exists because an earlier heal turned an
aligned non-pointer into a bogus heap address that then reached `free()`. `fork_verify.rs`'s
own module doc concedes the general case is "unbounded" and that a more ambitious attempt
"confirmed this the hard way."

**One thing worth keeping from this analysis even if B is rejected:** if any relocating path
survives as a fallback, **force `D` to a multiple of at least 64 MiB (preferably 1 GiB)**. That
eliminates `heap_for_ptr`, mimalloc's segment mask and CPython's pool mask by construction, for
free. It does nothing for safe-linking.

**Also worth knowing:** musl has none of this — no safe-linking, no mangling, raw atexit
pointers, and mallocng's `meta->mem == base` asserts make mistakes loud rather than silent. The
Alpine guests are not affected by 3N's class at all. This is a glibc-guest problem specifically.

---

## 5. What the current design costs, restated

The relocating fork is not merely incorrect; it is expensive and dangerous in ways worth
counting when weighing the migration.

- **Silent parent corruption.** Because fork only ever *adds* mappings, a stale pointer in the
  child does not fault — the parent's addresses stay live, mapped and executable. A child that
  `ret`s to a stale return address "genuinely resumes executing the parent's copy of the code,
  and any store that code performs … lands directly in the parent's live state"
  (`fork_verify.rs:17-24`). `D == 0` with separate address spaces removes this class outright.
- **Single-stepping.** `EFLAGS.TF` is armed for every forked child from resume until
  `execve`/`exit` (`fork_verify.rs:202`, `lib.rs:3621`), at 10³-10⁴× native. Fork-without-exec
  daemons — `dbus-daemon --fork`, xfsettingsd, Thunar `--daemon`, all in the XFCE path — never
  reach `execve`, so they run traced until a step bound cuts them off
  (`MAX_IDENTITY_VERIFICATION_STEPS` 4096 / `MAX_THREAD_VERIFICATION_STEPS` 16384). CPython was
  measured at 76,768+ traps and still climbing.
- **Bounded healing.** That step bound means coverage is time-limited as well as
  pattern-limited: past the bound, nothing is healed at all.
- **`MAP_SHARED` forks fail outright** (`mm/mod.rs:770`).
- **Deletion credit.** `fork_verify.rs` 2,658 + `process_fork.rs` 3,266 + `ctxwatch.rs` 591 =
  **6,515 lines**, roughly 35% of the Windows platform crate, plus **128** references to
  `AddressRelocations`/`ForkChildVerificationProvider` across 14 files (the figure of 44 in
  `ADVISORY-001` 1.2 item 6 is an undercount). The Linux and macOS platforms implement
  `ForkChildVerificationProvider` as an empty block — they get identical child addresses for
  free and never needed any of it.

Note also that under an identity map the healing machinery is not merely redundant but
actively harmful: `is_in_source` and `is_in_destination` cover the same range, so case (1)
fires on essentially every instruction and the child's whole execution degenerates into a
permanent trace (`fork_verify.rs:934-942`). Deleting it is required by the new model, not
optional tidying.

---

## 6. Recommendation

**Two independent tracks. Do not serialize them.**

**Track A — the demo, on existing code, no architectural change.** Per §1.5, pid-1 execution
and fork+exec are both already safe. Pursue clients-as-sibling-runner-instances against the
shared display, avoiding fork-without-exec entirely. This does not need anything below, and is
the fastest route to real pixels. Its ceiling is real, though: the XFCE session daemons
(`dbus-daemon --fork`, xfsettingsd, Thunar `--daemon`) fork without exec by design, so a full
stock desktop is not reachable this way — but a demonstrably working desktop surface with real
clients painting is.

**Track B — the architectural fix.** *Do not start an `RtlCloneUserProcess` project.* Finish
the cross-process fork that already exists, and adopt clone as the copy mechanism once the
state problem is solved.

Ordered, each step independently verifiable:

0. **Cheap decisive experiment, first, before anything else.** Run a stdio-only
   (`beyond_stdio == 0`) glibc fork repro under `LITEBOX_PROCESS_FORK=1` and confirm the
   `__libc_malloc+0x76` fault does not occur. This tests 3N's central claim against existing
   code with zero design change. Complement with `ADVISORY-001` 3N's own suggested
   `GLIBC_TUNABLES=glibc.malloc.tcache_count=0` run — expected to *move* the fault to fastbins,
   not remove it, which is itself the confirmation that option B is whack-a-mole.
1. **Presenter-process split** (`docs/presenter-process-design.md`), plus console I/O to
   file/pipe handles. Hard prerequisite; already designed; its one risk already retired.
2. **Cross-process `RawMutex`**: shared state word + per-waiter auto-reset `Event`, handle
   duplicated on demand and cached. One impl, ~150 lines, one file. Verifiable inside a single
   process with no behaviour change before any clone is involved.
3. **Fixed-base shared section behind `SafeZoneAllocator`**, holding `LiteBoxX` and
   `GlobalState`. Verifiable single-process first.
4. **Relax the `beyond_stdio` gate** as fd subsystems become shareable, one at a time — this is
   the actual unlock, and it is incremental. `pipes` and `net` first (they gate the most).
5. **Swap the copy leg to `RtlCloneUserProcess`.** At this point it is a contained change that
   buys lazy CoW, `MAP_SHARED` survival, identical handle values, and removal of the whole
   64 KiB reservation-group apparatus.
6. Signals via `QueueUserAPC2` (already probe-proven), `wait4`/exit through shared task state,
   SCM_RIGHTS across processes. Then delete the 6,515 lines.

Do **not** build the vfork-only shortcut as the general fork: the XFCE daemons fork without
exec and the "parent parked until child exits" semantic deadlocks them.

### How to think about the decision

The question as posed was "is `RtlCloneUserProcess` viable, and what would it take." The honest
answer is that the framing overestimates the API's importance and underestimates how far the
project already is. `D == 0` is solved. The address space was never the hard part — 64 KiB
granularity made it awkward and clone makes it easy, but both get there. The hard part is that
litebox's kernel is a pointer-rich Rust object graph in one process's private heap, and *that*
is what one-process-per-guest actually costs.

And per §1.5 the urgency is lower than it appeared: this is a fork-*without*-exec bug, so it
does not block a demo — only the general case. That is a reason to do Track B properly rather
than under deadline pressure, not a reason to skip it.

That cost is real but bounded and unusually well-contained for a change of this scope: one
allocator seam, one lock impl, one struct of 22 fields, and a fd gate that can be relaxed
incrementally rather than in a big bang. Set against ~6,500 lines deleted, a whole class of
silent parent-corruption bugs removed, a 10³-10⁴× slowdown removed on every fork-without-exec
daemon in the target workload, and `MAP_SHARED` forks working at all, it is worth doing.

The alternative — option B — is not a smaller version of the same fix. It is a different and
worse bet: it cannot reach the symbols it needs on a stripped libc, it breaks on glibc 2.42, it
has at least one sub-problem with no pointer to fix, and this repository has already run that
loop four times and narrowed coverage each time.

---

## 7. Open questions worth closing before committing

- Step 0's experiment. Nothing here should be funded before that result.
- Whether `spawn_cross_process_fork_child`'s child survives long-running execution, or only to
  `execve` — passes 142-157 built it; the archive's live evidence is all pre-3N.
- Reserve size and a collision-free high-VA band for the shared section: probe on 26200 rather
  than assume.
- `QueueUserAPC2` availability was listed as unverified in `ADVISORY-001` section 5, but
  `apc_probe.c` appears to discharge it; confirm the probe was actually run on this build.
- Byte-level confirmation of `PROTECT_PTR` in raw 2.41/2.42 `malloc.c` (secondary sources are
  consistent; not eyeballed against source).
