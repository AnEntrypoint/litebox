# AGENTS archive 2026-10-05a -- the GPU-process NULL: lazy bind and cold pages both refuted

Raw evidence behind the "Open item 1" rewrite in `AGENTS.md`. AGENTS.md keeps the rule; this file
keeps the numbers. Commit `a2ac697` was HEAD for these runs; the runner binary was newer than it.

## 1. cb40 -- is the GPU process's death a LAZY bind? (`-Run cb40`, `47cefb2`)

Three arms of the same headless screenshot (`/tmp/cb40.html`, `--virtual-time-budget=8000`,
`--window-size=800,600`, uid 911 via `setpriv`, `HOME=/tmp/cushot`), differing in one variable:

| arm | variable | PNG | notes |
|-----|----------|-----|-------|
| A | default (separate GPU process) | **NO_PNG** (poll i=30, 60 s) | zero bytes of guest stderr |
| B | `LD_BIND_NOW=1` | **NO_PNG** (poll i=30, 60 s) | zero bytes of guest stderr |
| C | `--in-process-gpu` | **PNG_APPEARED i=5**, 3737 bytes | dbus/udev noise only |

C producing its PNG makes the run valid. A and B behaving identically kills the lazy-PLT hypothesis:
with `LD_BIND_NOW=1` every binding is resolved before `main`, so there is no PLT stub left that could
jump to a resolver-returned 0. The NULL is reached by some other indirect call.

### The two fault snapshots are the same instruction (`.wfgy/cb40.err`, `logscan.py`)

```
pid=44  tid=44  comm=chromium Exception(14) kernel_mode=false
        rip=0x0 rsp=0x7fefffeea908 cr2=0x0 error_code=0x14
        rax=0x0 rdx=0x1 rcx=0x14000 rsi=0x1211c03ff6 rdi=0x0
pid=187 tid=187 comm=chromium Exception(14) kernel_mode=false
        rip=0x0 rsp=0x7fefffeea8f8 cr2=0x0 error_code=0x14
        rax=0x0 rdx=0x1 rcx=0x14000 rsi=0xa311c03ff6 rdi=0x0
```
Only `rsi` (and `rsp` by 0x10) differ; both `rsi` values sit ~573 KiB from the address the same
process passed to `readlink` as `"/proc/self/exe"` (`0x1211b7821d` / `0xa311b7821d`), i.e. inside one
mapping that still holds correct bytes. `error_code=0x14` = user-mode instruction fetch, `rip=cr2=0`.

Zero `panicked at`, zero `arena exhausted` in the 1 001 939-byte host log.

### The syscall trail (last 24, oldest first) is identical in A and B

```
i=0   clock_gettime(1, 0x7feffea8dd90, 0x300)
i=1   epoll_wait(3, 0x7feffea8df00, 0x10)
i=2   epoll_wait(3, ...)
i=3   openat(AT_FDCWD, 0x32e400004e70, 0x80000)          arg1_as_path=Some("")
i=4   newfstatat(AT_FDCWD, 0x7fefffeede8c -> "<CMDLINE>")
i=5   readlink(0x1211b7821d="/proc/self/exe", buf, 0xfff)
i=6   openat(AT_FDCWD, 0x32e400004e70, 0x80000)          arg1_as_path=Some("")
i=7   read(0xf, 0x7fefffeeb668, 0x340)                   <- 832 bytes = an ELF header
i=8   fstat(0xf, ...)
i=9   close(0xf)
i=10  gettid  i=11 gettid  i=12 gettid
i=13  mprotect(0x32e4000cc000, 0x4000,  0x3)
i=14  mprotect(0x32e4000d0000, 0x10000, 0x3)
i=15  mprotect(0x32e4000e0000, 0x4000,  0x3)
i=16  mprotect(0x32e4000e4000, 0xc000,  0x3)
i=17  mprotect(0x32e4000f0000, 0x8000,  0x3)
i=18  gettid
i=19  newfstatat(AT_FDCWD, "<CMDLINE>")
i=20  readlink("/proc/self/exe") -> "/usr/lib/chromium/chromium"   <- SUCCEEDS
i=21  openat(AT_FDCWD, 0x32e4000074b0, 0x80000)          arg1_as_path=Some("")
i=22  newfstatat(AT_FDCWD, "<CMDLINE>")
i=23  readlink("/proc/self/exe") -> "/usr/lib/chromium/chromium"   <- SUCCEEDS
      -> Exception(14) rip=0x0
```

Reading of it, and the caveats:

- The five `mprotect`s cover one CONTIGUOUS 176 KiB span (0xcc000+0x4000 = 0xd0000, ... 0xf0000+0x8000
  = 0xf8000) in five pieces, all to `0x3` (RW) -- five VMAs, so almost certainly the PT_LOADs of an
  object ld.so is mapping. Its `read(0xf, ..., 0x340)` just before is ld.so reading an ELF header.
  **So the GPU process dies inside ld.so while mapping a library, before binding anything** -- which
  is why cb21's `LD_DEBUG=libs` saw ZERO libraries for that pid.
- `arg1_as_path=Some("")` on the three `openat`s is a real 0x00 byte (a `None` would mean the host
  read faulted). But 0x32e400004e70 / 0x74b0 are image-relative offsets inside the first 32 KiB of
  the mapping (the mprotect addresses share the 0x32e40000 / 0x1d7c0000 prefix and the offsets are
  identical across the two runs despite different ASLR bases), i.e. inside the ELF header / program
  header table, where a 0 byte is unremarkable. **The "empty path" is therefore NOT established as a
  bug -- it may be the trail decoder printing whatever sits at an arbitrary pointer.**
- `<CMDLINE>` is the joined command line (`/usr/lib/chromium/chromium --type=gpu-process
  --headless=new --ozone-platform=headless --use-angle=swiftshader-webgl --crashpad-handler-pid=12
  --enable-crash-reporter=, --noerrdialogs --user-data-dir=/tmp/cu_A
  --change-stack-guard-on-fork=enable --gpu-preferences=YAAA...`) -- one string WITH spaces, so not
  `argv[]` (those are separately NUL-terminated) but a joined command line, at a STACK address
  (`rsp`+0x3584). Same shape as cb34's zygote `LD_DEBUG` tail, whose main-map `l_name` printed as
  the whole command line. So the object name ld.so is being asked to stat is not a path at all.

## 2. cb41 -- are COLD pages lost across a cross-process fork? (`-Run cb41`)

Every earlier content probe (cb14/cb15/cb30/cb38/cb39) read the mapping in the PARENT first and only
then re-read it in the child, so only WARM pages were ever tested. cb41 mmaps the file, lets the
parent touch only the first `warm` of 32 spread offsets, forks, and has the CHILD read all 32 against
its own `pread` of the file.

| arm | file | prot | warm | child bad_warm | child bad_cold |
|-----|------|------|------|----------------|----------------|
| 1 | chromium (324 854 126) | R | 0 | 0/0 | **0/32** |
| 2 | chromium | R | 8 | 0/8 | **0/24** |
| 3 | chromium | R | 32 (control) | 0/32 | 0/0 |
| 4 | chromium | R\|X | 0 | 0/0 | **0/32** |
| 5 | libc.so.6 (2 008 605) | R | 0 | 0/0 | **0/32** |
| 6 | libc.so.6 | R | 8 | 0/8 | **0/24** |
| 7 (gen 2) | chromium | R | 0 | 0/0 | **0/32** |

Every parent `CONTROL bad=0/32`, every `status=0` (no child died, no SIGSEGV, handler never fired),
`PY_RC=0`. **Cold file pages are inherited byte-exactly, at generation 1 and 2.**

These forks are genuinely cross-process -- `cb41.err` carries
`spawn_cross_process_fork_child: parent-side spawn (memory copy, process creation) took this long
elapsed_ms=33..55` and each child logs
`vmem-adopt-probe (child): adopting 87 pre-populated region(s) ... PROT_NONE reserved and tracked=69
... VMA layout adoption VERIFIED -- every region's boundaries, flags and file-backing round-trip
exactly, no allocation performed`.

## 3. What is excluded, with the run that excluded it

- **lazy PLT bind returns 0** -- cb40 arm B (`LD_BIND_NOW=1`), identical fault.
- **cold/unfaulted file pages commit as zeros in a fork child** -- cb41, all seven arms.
- **warm file pages, RX mappings, fragmented address spaces** -- cb38/cb39 (`bad=0/32` everywhere).
- **wrong-file delivery, zeroed anonymous/malloc/heap memory** -- cb32 (`bad=0`, 98 opens x 2 x 2).
- **stale absolute pointers after a fork** -- cb36/cb37.
- **a fork child cannot `dlopen`/`dlsym`** -- cb30/cb31/cb33 (non-null at generation 0/1/2).
- **chromium's sandbox** -- cb21 L1, the GPU pid loads zero libraries.
- **`gcc`-compiled probes** -- impossible, there is no C compiler in the image (`gcc` -> rc 127).

## 4. What to do next

Identify WHICH library the GPU process is mapping when it dies, then bisect the GPU work with flags
(`--disable-features=Vulkan`, `--use-angle=...`). Note for the next probe: arms A and B produced zero
bytes of guest stderr, so per-pid attribution has to come from the host log or from `LD_DEBUG` on the
whole tree into one file -- `LD_DEBUG_OUTPUT` is not usable here, because fork children inherit the
fd and write into their exec-ancestor's file (cb34 saw only `ld.1` and `ld.10`).
