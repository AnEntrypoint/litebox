# 2026-10-04i -- the separate GPU process: what dies, and what does NOT explain it

Companion to `AGENTS.md`. Everything here is measured, with the run that produced it. Shorthand as in
`AGENTS.md`.

## The headless `--screenshot` question is answered: it completes iff the GPU runs IN-PROCESS

`cb2` (two arms, one at a time, chromium non-root on a HOME nothing root touched, page served by an
in-guest httpd on 8082, output piped to a python summarizer that stops at its own wall deadline so no
`head` SIGPIPE can kill the arm):

- **arm B `--in-process-gpu`: `PNG_APPEARED i=8`, `HAS_PNG`.** 17004 lines captured. Its `LD_DEBUG`
  contains `17: calling init: /usr/lib/chromium/libvulkan.so.1` -- the bundled loader loads in the
  browser process, and the ICD search that follows (`libvulkan_gfxstream.so`,
  `libvulkan_intel_hasvk.so`, `libvulkan_intel.so`, ...) is the loader doing its normal work.
- **arm A (default): no PNG, ever.** The GPU process (guest pid 192) prints exactly three lines --
  `[192:192:...] No entry found for gpu-process/global`, `.../gpu-process/*`, `.../gpu-process/main`
  -- and nothing more. `libvulkan` is never initialized in ANY process. The browser and the network
  service keep logging (cache stats, GCM registration) for the rest of the run: **chromium hangs
  waiting on a GPU process that is already dead.** `sg1` had already proved reaping works (a faulting
  fork child is reaped in 0.21s at gen 0/1/2, killed by signal 11), so "the parent never learns" is
  not the explanation for the hang.

So open item 1 ("headless `--screenshot` never completes") is not a headless bug at all: it is the
separate-GPU-process bug. With `--in-process-gpu` the screenshot completes in ~16s of polling.

## What the dying process is, exactly (`chrshot8`, `47cefb2` + this pass's diag)

`chrshot8.err`: `Exception(14) rip=0x0 cr2=0x0 error_code=0x14` in `comm=chromium pid=53 tid=53`
(winpid 132212, a cross-process fork child). `0x14` = instruction fetch from a NON-PRESENT page, i.e.
a jump to 0. One such fault in the whole run, and chromium printed no crash message afterwards.

The pre-signal syscall trail (24 entries, `arg1_as_path` added this pass so `openat`/`newfstatat`/
`readlink` name their argument), oldest first:

```
i=0  gettid
i=1  newfstatat(AT_FDCWD, <stack buf holding the whole command line>, ...)
i=2  readlink("/proc/self/exe", buf=0x7fefffeead60, 0xfff)
i=3  openat(AT_FDCWD, "", O_CLOEXEC)          <-- empty path
i=4  newfstatat(same as i=1)
i=5  readlink("/proc/self/exe", 0x7fefffeead60)
i=6  openat(AT_FDCWD, "", O_CLOEXEC)
i=7  read(fd=0xf, 0x340)                       <-- 832 bytes: ELF header + program headers
i=8  fstat(0xf)     i=9  close(0xf)
i=10..12 gettid
i=13..17 mprotect(0x564000cc000,0x4000,3)  mprotect(0x564000d0000,0x10000,3)
         mprotect(0x564000e0000,0x4000,3)  mprotect(0x564000e4000,0xc000,3)
         mprotect(0x564000f0000,0x8000,3)      <-- a fresh 144 KiB object being mapped
i=18 gettid   i=19 newfstatat   i=20 readlink("/proc/self/exe")  i=21 openat("")
i=22 newfstatat  i=23 readlink("/proc/self/exe")
```

i=1/i=4/i=19/i=22 all `newfstatat` the SAME stack buffer, and that buffer holds the whole command
line -- which is how the process was identified as `--type=gpu-process`. `readlink`'s buffer reads
back as `/usr/lib/chromium/chromium` at i=20/23 and as garbage at i=2/5: the dump happens at fault
time, so a buffer reused since the call shows stale bytes. **`arg1_as_path` is only trustworthy for
the most recent entry of a given address** -- a lesson for every future trail read.

The faulting instruction (from `[rsp]`, the return address a `call *reg` pushed):

```
call site 0x1b1390534e  (mapping 0x1b12c52000-0x1b229ae000, offset 0xcb334e, VM_READ|VM_EXEC)
... 8d 35 6f 59 17 fe | 48 89 c7 | e8 f4 25 0a 0f | 48 89 05 8d 6e cd 0f | 48 8d 35 ac ec 2f fe | 31 ff | ff d0
    lea rsi,[rip+..]     mov rdi,rax  call dlsym    mov [rip+..],rax           lea rsi,[rip+..]        xor edi,edi  call *rax
```

i.e. `gpa = dlsym(handle, "vkGetInstanceProcAddr"); gpa(NULL, "vkCreateInstance")` with `gpa == 0`.
`pv5` named both strings from chromium's own rodata: file offset `0x1A7ACA3` = "vkGetInstanceProcAddr",
`0x1C03FF6` = "vkCreateInstance". `pv3` had shown "TryMigrateInstance" is in chromium, not SwiftShader.

**The rodata is intact.** The register dump at fault time reads at `rsi=0x1b11c03ff6`:
`63 65 00 "TryMigrateInstance" 00 "vkCreateInstance" 00 "WebGLM"...`. So the NULL is not a zeroed
string and not a zeroed rodata -- it really is a symbol lookup that found nothing.

Note the trail has no `mmap`: the object being mapped (i=7..17) was opened, read, fstat'd and closed,
so a `dlopen` was in flight or had just finished when the call to `gpa` was made.

## Ruled out this pass (all by measurement)

- **Anonymous memory across a cross-process fork is clean.** `cb1`/`an1`: `MAP_PRIVATE|MAP_ANONYMOUS`
  8 MiB, a 4 MiB heap `bytearray`, `MAP_SHARED|MAP_ANONYMOUS` 1 MiB, carried through generations
  0/1/2 and verified against a COMPUTED pattern `pat(i) = (i*7+13) & 0xFF` (not an inherited copy,
  which a zeroing bug would also zero): `bad_chunks=0 zero_chunks=0` at every generation. The fourth
  case, "dirty private file pages", reported `bad_chunks=64` at GEN0 too -- a probe bug (it compared
  a 4 MiB range against the pattern while only writing 4 KiB per 1 MiB), not a fork bug.
- **Chromium's bundled Vulkan loader is fine in fork children.** `cb1`/`pv7`:
  `/usr/lib/chromium/libvulkan.so.1` (487280 bytes, SONAME `libvulkan.so.1`, NEEDED only `libc.so.6`,
  exports `vkGetInstanceProcAddr` and `vkCreateInstance`, does NOT export `vk_icdGetInstanceProcAddr`)
  dlopens under `RTLD_NOW` and `RTLD_LAZY` and resolves at generations 0, 1 and 2, by absolute path
  and by bare name with `cwd=/usr/lib/chromium`; the system loader resolves too. `pv3` had already
  shown this for the system loader. So "dlopen is broken in fork children" is false.
- **`/proc/self/exe` is correct in fork children.** `pv4`: `readlink("/proc/self/exe")` returns
  `/lsiopy/bin/python3` at generations 0-3. (It also found two real gaps: `/proc/<pid>/exe` is EINVAL
  and `/proc/self/comm` is ENOENT -- `/proc/self/comm` appears nowhere in the Rust tree at all; the
  only hit for the string in the repo is `advisor/probes/getenv_probe.c`.)
- **fd 2 is carried.** `cb1`/`fd2`: a gen-1 and gen-2 child's writes to fd 2 through a PIPE all arrive,
  in order, both via `os.write(2, ...)` and via `sys.stderr`. A subagent pinned the mechanism:
  `process.rs:3684-3687` excludes only *plain* stdio from the carry spec (it rides `CreateProcessW`
  inheritance), and `process_fork.rs:4340-4385` wires `hStdError` from the spawning process's
  `GetStdHandle(STD_ERROR_HANDLE)` -- so a fork-of-a-fork inherits it too. Caveat recorded for later:
  `process_fork.rs:4390-4392` leaves a child's stdio completely unset when
  `!want_stdout_pipe && !inherit_stdio`.
  **But the FILE destination lost the gen-1 child's lines** and emitted a stray `BJ` fragment while
  the parent's lines survived -- a fork child's writes to a regular file are lossy. Unexplained;
  worth its own probe (chromium writes files constantly).
- **The fatal loader errors are NOT the discriminator.** Both arms print, from the browser pid:
  `symbol lookup error: undefined symbol: localtime64 (fatal)`, `... localtime64_r (fatal)`,
  `... nspr_use_zone_allocator (fatal)`, right after `find library=libnspr4.so` / `libnss3.so` /
  `libcap-ng.so` / `libglib-2.0.so.0` with a `trying file=` and no `calling init:`. Arm B gets a PNG
  anyway, so these are not what kills arm A.
- **No silent zeroing of file-backed mappings.** `zf1`/`gf1` (byte-identical through generation 3,
  inherited and fresh) plus the intact rodata above.

## Open, narrowed

1. **Which dlopen in the GPU process yields the NULL handle**, given that the same dlopen succeeds in
   a python fork child at gen 0-2 and in the browser process in-process. `cb4` targets it: a per-pid
   histogram plus EVERY line the gpu-process pid emitted.
2. Why chromium HANGS instead of falling back after the GPU process dies (reaping works; the browser
   keeps logging for minutes).
3. `/proc/<pid>/exe` EINVAL and `/proc/self/comm` ENOENT (absent from the tree entirely).
4. A fork child's writes to a regular file are lossy (`cb1` fd2 file case).
5. Carried over: the exec-bit loss below.

## Exec-bit loss -- the lazy-map seed was wrong, but the A/B says it was NOT chrD83

`lazy_file_map::register` seeded every lazy range's `protection` at `PAGE_READWRITE` no matter what
the guest mapped. A file mapped `PROT_READ|PROT_EXEC` that demand-filled would therefore come back
WRITABLE BUT NOT EXECUTABLE -- and the guest's first call into it faults as an instruction fetch from
a present page, Windows `Exception(14) error_code=0x15`, the chrD83 signature. Fixed: `register` now
takes the mapping's real protection (`prot_flags(permissions)`, threaded through
`PageManagementProvider::try_lazy_file_pages` from `do_mmap_file_memcpy`, where `prot` is in scope).

**An A/B refuses to credit that with fixing chrD83.** `cb8` runs the same probe on both binaries with
`LITEBOX_DIAG_LAZY_FILE_MAP=1` on, and the pre-fix binary passes `EXEC_RX` at every generation too.
Something re-applies the real protection (`permission_update_ranges`, via `do_mmap`'s own
`update_permissions`) before the first fill, so the seed is corrected in time. The change is kept
because the seed was simply a lie about the mapping, and because it also stops a `PROT_NONE` and a
`PROT_READ` lazy mapping from filling read-write -- but **it is not proven to change any observable
behaviour, and chrD83 stays open** (and un-re-measured on a clean host, which AGENTS.md already
requires before calling it a litebox bug at all).

## How the exec-bit probes had to be built (three blind ones first)

- `cb3` used `nm -D`, which is NOT in the image, so `IMG_PURE_RX` -- the only case that can tell the
  buggy code from the fixed code -- printed `SKIPPED`. It also only ran generations 0 and 3.
- `cb6` resolved symbols by hand and got `symbol-not-found`: **`strlen` in glibc is an IFUNC**
  (`STT_GNU_IFUNC` == 10), not `STT_FUNC` == 2. Calling one from a fresh, UNRELOCATED private mapping
  would fault through a zeroed GOT for reasons that have nothing to do with the bug, so that road is
  a dead end regardless.
- `cb7` tried writing to an `RX` mapping instead (writable == buggy). That is only a valid
  discriminator if litebox raises SIGSEGV from the host access violation; a write path that checks
  the guest VMA flags first would report OK either way. Never resolved.
- `cb8` measures the execute bit directly, which no VMA-flag check can mask: it scans the mapped page
  for a lone `0xC3` (`ret`) and calls THAT address. A call whose first byte is `ret` pushes a return
  address and pops it straight back -- no GOT, no PLT, no globals, no side effects -- and it faults
  if and only if the page is not executable. Cases: `EXEC_RX` (must not fault), `EXEC_R` (negative
  control: mapped `PROT_READ`, must fault -- it does, so the probe is sensitive), `RX_FILE` and
  `RW_FILE` (write checks). 16/16 on both binaries. `EXEC_PAGE_OFFSET 0x28000`, `GADGET at=+1131`.

**Probe lesson: a probe that cannot fail is not evidence.** Every one of cb3/cb6/cb7 could have
printed all-OK while being blind to the thing it claimed to test; only `EXEC_R` in cb8 proves
otherwise. Keep a negative control in every protection probe.

**And cb8 still does not test the chrD83 hypothesis.** It creates each mapping in the process that
runs the case, so it measures `mmap`'s own path but never lets a mapping CROSS a fork; every mapping
it tests was created after the last fork. chrD83's actual claim is `Vmem::duplicate` re-narrowing
protection as it copies a parent's VMA into a child (`mm/linux.rs:1839-2164`, prot re-narrowed at
`:2154`). `cb9` maps in the parent and executes in the child, at fork generations 1-3, in two
variants -- `*_CLEAN` (the child is the one that demand-fills, i.e. the child re-arms from the fork
descriptor and `adopt_from_file` restores whatever protection the parent recorded) and `*_TOUCHED`
(the child inherits an already-filled range) -- with `INH_R_*` as the negative control and
`INH_RW_W` proving the inherited range is mapped at all.

## A probe bug that turns a TIMEOUT into a crash (`inetfork1.sh`, `inet1x.sh`)

`socket.timeout` IS an `OSError` whose `errno` is `None`, so `ose()`'s `"errno=%d %s" % (exc.errno,
...)` raises `TypeError` INSIDE the except block. The verdict line is then replaced by a traceback:
a plain 30 s recv timeout reads as a crashed probe, and the one detail that mattered -- that it was
a timeout -- is exactly what gets lost. `reg1c` showed precisely that shape (a traceback where
`[inet.tcp_child_to_parent_parentside] FAIL` should have been). Fixed to `%s` in both probes.

## Diagnostics added this pass (`litebox_shim_linux/src/lib.rs`)

- `arg1_as_path` on every syscall-trail entry (names the file an `openat`/`newfstatat`/`readlink`
  was called on).
- `[rsp-8]` AND `[rsp]` reported as candidate call sites, with the mapping containing each and the
  32 bytes immediately before the call site -- the byte dump is what names `ff d0` (`call *rax`).

## Carried TCP: who gets the bytes (the rx-theft bug)

**Symptom.** A TCP connection carried across a cross-process fork intermittently LOSES data:
`cb10` (6 iterations, parent sends P1 then P2 while the child is already blocked in `recv`) gave
`it5 GOT1 b'P2-5' after 3.0s / GOT2 TIMEOUT` -- the child woke on the SECOND message and its next
recv found nothing. `inet1x` it3 lost the parent's reply the same way. `cb11` added the decisive
measurement -- after reaping the child, read the PARENT's side of the same socket:

    it1 GOT1 b'P2-1' after 3.0s / GOT2 TIMEOUT after 20.0s / it1 PARENT_LEFTOVER b'P1-1'
    it4 GOT1 b'P2-4' after 2.9s / GOT2 TIMEOUT after 20.0s / it4 PARENT_LEFTOVER b'P1-4'

2/6 lost, and the missing bytes were sitting in the PARENT's proxy.

**Mechanism.** A carried connection is ONE smoltcp socket with two referents: the parent's
`SocketHandle` and the child's borrowed one (`fork_adopt`'s `"T"`/`"U"` arms, `borrowed: true`),
and `install_inet_at_fd` gives the child its OWN `StreamSocketChannel`
(`GlobalStateHandle::initialize_socket`). The proxy is a per-process object (it lives in the
descriptor entry; its rings are ordinary heap). `drain_all_socket_channel_buffers`
(`litebox/src/net/mod.rs`) runs in EVERY process of the fork family and moves rx out of the shared
smoltcp socket into whatever proxy IT holds -- so the bytes land in whichever referent's tick ran
first, and if that is not the process that is reading them they are unreachable: the reader's
proxy stays empty and its `wait_on_events_polling` re-poll cannot recover them (it only re-reads
its own proxy).

**Fix (two halves, both needed).**

1. **The tick only delivers to a referent that is waiting.** `Network::shared_across_fork`
   (`[Option<SocketHandle>; MAX_SOCKETS]`, shared arena, marked by `fork_adopt`'s borrowed arms,
   never cleared -- a stale mark only costs a pull) plus `Subject::has_observers()`
   (`Pollee::has_observers()`, the atomic `nums`, deliberately no lock: over-reporting just keeps
   the old behaviour). `drain_socket_channel_buffers` skips the TCP **and** UDP rx drain when
   `shared_across_fork && !proxy.has_observers()`, so a non-reading referent cannot steal.
2. **A reader pulls for itself.** Half (1) alone would break a NON-BLOCKING read on a shared
   socket: it registers no observer, so nothing would ever fill its proxy. So `receive`
   (`litebox_shim_linux/src/syscalls/net.rs`) calls `Network::drain_rx_into_proxy(fd)` before
   `try_read` when the proxy is marked shared (`is_shared_across_fork`, an atomic on the channel
   set by `set_socket_proxy` for a borrowed handle and by the tick for the other referent), which
   runs the drain for that one fd with the gate off.

Single-referent sockets keep the exact old path: `shared_across_fork == false` leaves the gate
inert and no pull happens, so no extra `net_lock` is taken on an ordinary `recv`.

**Known residual hole.** If BOTH referents have a live observer (both blocked in a read on the
same carried socket, or an `epoll` registration in each process), both drains run and the race is
still there. Not measured, not closed.

**Measured after the fix (`cb12` = `cb11` re-run, `cb13`, exe 21:30).** Blocking: 12/12 clean
over the two runs -- every iteration `GOT1 b'P1-n'` / `GOT2 b'P2-n'` / `PARENT_LEFTOVER none`,
zero 20 s timeouts (was 2/6 lost). Non-blocking (`cb13` case B): 6/6
`NB_GOT b'NB1-n;NB2-n;END'` in ~3 s each, `PARENT_LEFTOVER none` -- so the pull half works, which
is exactly what a "only drain for a waiter" rule alone would have broken. Both `.err` logs: zero
`panicked at`, zero `error_code=`.

**Probes.** `.wfgy/cb10.sh` (blocking child, no parent-side leftover read), `cb11.sh` (adds
`PARENT_LEFTOVER`), **`cb13.sh` (blocking AND non-blocking readers, 6 iterations each -- the
non-blocking case is what proves half (2))**, `inet1x.sh` (child sends, parent reads, parent
replies, child reads).
