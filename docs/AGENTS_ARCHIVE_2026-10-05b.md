# AGENTS archive 2026-10-05b -- cb42: the GPU process's loader state is CORRUPTED, not missing

Raw evidence behind the "SURVIVING" bullet of open item 1 in `AGENTS.md`. `a2ac697` was HEAD; the
runner binary was newer than it.

## 1. cb42 arm table

Same headless screenshot as cb40 (`/tmp/cb42.html`, `--virtual-time-budget=8000`,
`--window-size=800,600`, uid 911 via `setpriv`, `HOME=/tmp/cushot`), 45 x 2s poll per arm:

| arm | variable | PNG | grep-filtered log lines |
|-----|----------|-----|-------------------------|
| A | default + `LD_DEBUG=symbols` | **NO_PNG** (i=45) | 35 |
| B | `--disable-features=Vulkan,VulkanFromANGLE,DefaultANGLEVulkan` | **NO_PNG** (i=45) | 0 |
| C | `--in-process-gpu` (control) | **PNG_APPEARED i=7**, 3724 bytes | 0 |

Two conclusions: **vulkan is NOT the discriminator** (removing it changes nothing), and the run is
valid (C produced its PNG).

## 2. What pid 51 (the GPU process) actually searched

`LD_DEBUG=symbols` prints one line per object SEARCHED. The complete record for the GPU process:

```
51: symbol=vkGetInstanceProcAddr;  lookup in file=/lib/x86_64-linux-gnu/libnssutil3.so [0]
51: symbol=vkGetInstanceProcAddr;  lookup in file=/lib/x86_64-linux-gnu/libplc4.so [0]
51: symbol=vkGetInstanceProcAddr;  lookup in file=/lib/x86_64-linux-gnu/libplds4.so [0]
51: symbol=vkGetInstanceProcAddr;  lookup in file=/lib/x86_64-linux-gnu/libnspr4.so [0]
51: symbol=vkGetInstanceProcAddr;  lookup in file=/lib/x86_64-linux-gnu/libc.so.6 [0]
51: symbol=vkGetInstanceProcAddr;  lookup in file=/lib64/ld-linux-x86-64.so.2 [0]
51: /lib/x86_64-linux-gnu/libnssutil3.so: error: symbol lookup error: undefined symbol: vkGetInstanceProcAddr (fatal)
    ... (identical six lines + error again)
```

That is the NSS/NSPR set plus libc and ld.so -- **not** a GL/Vulkan scope, and cb21 showed that pid
loads ZERO libraries. The searchlist it walks belongs to some other object.

## 3. Two more lines from the same run, both about WRITABLE state

```
11: /usr/lib/chromium/chromium: error: symbol lookup error: undefined symbol: localtime64 (fatal)
11: /usr/lib/chromium/chromium: error: symbol lookup error: undefined symbol: localtime64_r (fatal)
```
repeated for pid 1 under four different `--type=` command lines. cb14's dynsym dump says chromium
**defines** both symbols, so the lookup is failing against a symbol table it cannot read correctly.

```
55: symbol=keyctl_restrict_keyring;  lookup in file=/proc/self/exe/usr/lib/x86_64-linux-gnu/gio/modules/libdconfsettings.so: error: symbol lookup error: undefined symbol: g_module_unload [0]
```

`/proc/self/exe` immediately followed by `/usr/lib/...` with no NUL between them: one buffer holds
the tail of one writer's string and the head of another's. Two writers, one page.

## 4. What that points at, and the gap it exposes

ld.so builds `l_name`, the search-path arrays and the `link_map` chain in writable memory (heap and
`.data.rel.ro`). If a page holding them is SHARED when it should be COW-private, two processes'
loaders write over each other, and every symptom above follows at once: a scope belonging to a
different object, symbols that exist reported undefined, and two paths in one buffer.

**No probe before this one ever WROTE in a fork child.** cb14/cb15/cb30/cb32/cb38/cb39/cb41 all
mapped or allocated in the parent and only READ in the child; cb41 additionally proved cold file
pages arrive byte-exact, and cb32 proved anon/malloc/heap CONTENTS are intact. Write ISOLATION was
untested. `.wfgy/cb43.sh` closes the gap: parent fills a pattern, the fork child overwrites it and
blocks on a pipe handshake while the parent re-reads, so `LIVE_SHARE` (visible while the child is
alive) is separated from `EXIT_CLOBBER` (visible only after `waitpid`, i.e. the writable-layer
import at reap, `9412184`). Arms: 4 MiB anon, 64 MiB scratch file, the 324 MiB chromium binary,
8 MiB malloc'd, and a generation-2 grandchild as the writer.

## 5. Per-process/thread state the cross-process child does NOT restore (read-only audit)

1. **`CLONE_CHILD_SETTID` is never honored on the cross-process path.** `process.rs:5321` builds
   `set_child_tid`, consumed only by `ThreadInitState::ForkedChild` (`:6467` -> `:9179-9187`); the
   cross-process branch returns at `:6402` first. glibc's `_Fork` passes `ctid = &THREAD_SELF->tid`,
   so the child's byte-copied TCB holds the PARENT's tid while the shim's `gettid` returns the
   child's real one. `clear_child_tid` is likewise only translated at `:6455`, after that return.
   cb40's trail has four `gettid` calls just before the fatal `mprotect` run.
2. **TCB repair offsets are musl's, not glibc's** (`process.rs:6157-6158`: `TCB_PREV_OFFSET=0x10`,
   `TCB_NEXT_OFFSET=0x18`). In glibc's `tcbhead_t` 0x10 is `self` (benign) but 0x18 is
   `multiple_threads` + `gscope_flag` -- an 8-byte pointer write there clobbers both. Latent: the
   block is gated off by `child_fs_base_verified_in_destination` (`:6098`).
3. **Stale-pointer healing is skipped for this child** (`fixup_stale_elf_data_pointers`,
   `process.rs:5869`, not applied at `:5963-5973`); only `fork_verify`'s reactive single-step covers
   it, and only for pointers dereferenced within the step budget.
4. **vdso/vsyscall: none** (`platform/lib.rs:15194-15197`), so `AT_SYSINFO_EHDR` is absent for
   parent and child alike -- not fork-specific.
5. **No re-run of ld.so/ELF entry**: the child resumes at the snapshot `rip` with `rax = 0`.
6. **Adopted regions can end up with different flags than the parent had**
   (`Vmem::new_adopting_existing_memory` -> `adopt(..., keep_all=false)`, `mm/linux.rs:991-997`):
   shared/inaccessible regions are skipped (`:1108-1109`) and `PROT_NONE` spans are re-`VirtualAlloc`'d
   `PAGE_NOACCESS` (the log's `tracked=69`). linux.rs:1042-1044 already records the resulting class:
   a later guest `mprotect` into such a span yields a mapping whose bytes are not the parent's, so a
   function-pointer slot read out of it is `0x0` -- an indirect call through NULL.
7. **`FsState::new()` (`litebox_shim_linux/src/lib.rs:1324`)** gives the child a fresh cwd/root/umask,
   so relative and `$ORIGIN` resolution differs from the parent's (matches the bogus path stat'ed).


## 6. cb45: the measured `mprotect` EACCES, and what fixed it

`.wfgy/cb45.sh` maps 8 MiB at `/tmp/cb45.bin` (or anon) and walks a protection sequence, printing
`errno`, in the parent and then in a cross-process fork child:

| arm | shape | BEFORE | AFTER (`prot.is_empty()` excluded) |
|-----|-------|--------|------------------------------------|
| A1 | anon RW -> NONE -> RW | rc=0 | rc=0 |
| A2 | anon NONE -> RW | rc=0 | rc=0 |
| F1 | file RW -> NONE -> RW | rc=0 | rc=0 |
| F2 | file NONE -> RW | **rc=-1 EACCES** | rc=0, write sticks |
| F3 | file NONE -> R | rc=0 | rc=0, reads the file's `0x5a` bytes |
| F4 | file R -> RW | not reached (probe bug: it wrote before the mprotect) | still OPEN |

Host log for the F2 refusal: `vma_flags_bits=89 vma_file_backed=true vma_shared=true
requested_flags_bits=3`. 89 = `VM_READ|VM_SHARED|VM_MAYREAD|VM_MAYEXEC`; 3 = `VM_READ|VM_WRITE`;
`VM_WRITE` is the bit with no `VM_MAYWRITE` behind it. cb44's `P1_scratch_protNone` and
`P3_chromium_protNone` arms printed `CHILD mprotect_RW_FAILED` for the same reason.

**It is NOT the GPU killer**: `.wfgy/cb40.err` (1,001,939 bytes) contains `refusing mprotect` **0**
times and `mprotect` 10 times, i.e. all five of the dying GPU process's `mprotect(len, 0x3)` calls
SUCCEEDED. Independent defect.

## 7. cb47/cb48: heap, generation 2, and a lookup through an inherited `link_map`

The GPU process is the zygote's child, i.e. generation 2. Before this pass nothing had ever written
in a fork child at gen 2 (cb43's own g2 arm never ran) and nothing had tested the HEAP (cb43's heap
arm died on a freed ctypes buffer) -- which is where ld.so's `link_map`/`l_info`/scopes live.

cb48 (every wait bounded at 20 s, so "never started" / "cannot be reaped" / "stuck" are distinct):

```
[LOOKUP_gen1/gen2] dlsym getpid/time/strlen/qsort/gettimeofday/strtol -> SAME as the parent,
                   call getpid ok, nss3 NSS_VersionCheck=1 in both
[G1_anon4m]        child: sees_A bad=0, wrote_B bad=0 WRITE_STICKS; parent live/after_exit/late clean
[G2_anon4m]        grandchild pid=9 ppid=8: sees_A bad=0, wrote_B WRITE_STICKS;
                   INTERMEDIATE_REAPED=0, PARENT_REAPED=0, parent clean at every check
[G2D_anon4m]       intermediate exits WITHOUT waiting; DETACHED grandchild pid=11 still runs, writes,
                   parent reaps the intermediate and is clean live/after_exit/late
```

So: heap writes are isolated, generation 2 forks, runs and is reaped, and `dlsym` through an
INHERITED `link_map` returns the parent's exact addresses and calls cleanly. (cb47's gen-2 arm
appeared to hang, but every wait in it was unbounded; with bounds it completes in seconds -- do not
read that as a gen-2 hang.) Note the pid numbering: gen-1 child pid 4/7, gen-2 grandchild 9 with
ppid 8, so each generation really is a new host process.

## 8. Fork-placement mechanics (moved out of `AGENTS.md` to keep it under 30 kb)

Fork-placement mechanics, for when it matters: `Vmem::duplicate` (`mm/linux.rs:1839`) byte-copies a private file-backed region into a fresh anonymous mapping at `group_dest_base + (range.start - group_source_base)` (`:2059`), `group_dest_base` from `insert_mapping(..., Hint)` (`:1934`), `max_intra_group_gap` 64 MiB (":a known allocator-specific fragility", `:1877`). `fixup_stale_elf_data_pointers` (`process.rs:2350`, called `:5869`) rewrites 8-byte-aligned values through `AddressRelocations::translate` (`mm/mod.rs:370`) but only inside `private_data_ranges()` (private && writable && !executable && !stack), and is **NOT applied to the cross-process child at all** (`process.rs:5963-5968`) -- where placement is already IDENTITY (`copy_one_group`, `fork.rs:4902`, force-reserves the exact source span), so no fixup is needed unless that reserve fails.

## 9. Open item 1: the exclusion list, verbatim (moved out of `AGENTS.md`)

**EXCLUDED BY MEASUREMENT (do not re-test): bytes** -- `pread == mmap`, 5 libs incl. the 324MB binary, parent + 2 fork children (cb14/cb15), and an inherited 324MB `PROT_READ|PROT_EXEC` mapping read at 32 offsets with 0/20/40/60 preceding anon mappings is `bad=0/32` in parent AND child (cb37/cb38/cb39); **symbols** -- `LD_BIND_NOW=1 chromium --version` rc=0, `ldd -r` clean, `dlopen`+`dlsym("vkGetInstanceProcAddr")` NON-NULL at gen 0/1/2 (cb30/cb31/cb33); **wrong-file delivery** -- 98 opens x 2 rounds x 2 gens `bad=0` incl. `libnssutil3.so` and `libvulkan.so.1` (cb32); **zeroed anon/malloc/heap** -- 64MiB + 32MiB + 200k objects `bad=0` (cb32); **stale absolute pointers** -- a chromium-shaped child keeps every address and dereferences a heap-stored pointer across the fork (cb36/cb37); **the sandbox** -- cb21 L1, that pid loads ZERO libraries.
