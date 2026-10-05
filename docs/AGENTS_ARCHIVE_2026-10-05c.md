# AGENTS.md archive 2026-10-05c

Mechanism prose removed from `AGENTS.md` at the `-05c` recompact, plus the cb60-cb66 trails that
closed the GPU-process bug. Every RULE these trails produced stays in `AGENTS.md`; this file only
answers "how was that measured".

## 1. The ex-"Open item 1": headless `--screenshot` completed only with `--in-process-gpu`

CLOSED by `f345821` (cb66 arm Z). The block below is what `AGENTS.md` used to carry, verbatim:

> **HEADLESS `--screenshot` COMPLETES IFF THE GPU RUNS IN-PROCESS (cb2/cb4b, `47cefb2`).**
> `--in-process-gpu` -> `PNG_APPEARED i=8`, `HAS_PNG` 800x600 `#20c0f0=99.75%`; default
> (separate GPU process) -> NO PNG: the GPU process prints three lines
> (`No entry found for gpu-process/{global,*,main}`) and dies with
> `Exception(14) rip=0x0 cr2=0x0 error_code=0x14 rax=0x0` = `call *rax` through NULL.
> **Not a headless bug -- the separate-GPU-process bug.**
> - **EXCLUDED BY MEASUREMENT (do not re-test): file bytes** (`pread == mmap`, 5 libs incl. the
>   324MB binary, parent + 2 fork children; an inherited 324MB `PROT_READ|PROT_EXEC` mapping read
>   at 32 offsets is `bad=0/32` in parent AND child); **symbols**
>   (`LD_BIND_NOW=1 chromium --version` rc=0, `ldd -r` clean,
>   `dlopen`+`dlsym("vkGetInstanceProcAddr")` NON-NULL at gen 0/1/2); **wrong-file delivery**
>   (98 opens x 2 rounds x 2 gens `bad=0`); **zeroed anon/malloc/heap**; **stale absolute
>   pointers**; **the sandbox** (that pid loads ZERO libraries).
> - **cb40 KILLED THE LAZY-BIND LEAD; cb41 KILLED THE COLD-READ LEAD.** cb40: A (default) and B
>   (`LD_BIND_NOW=1`) both NO_PNG and fault at the IDENTICAL snapshot
>   (`rip=0x0 cr2=0x0 error_code=0x14 rax=0x0 rdx=0x1 rcx=0x14000 rdi=0x0`; only `rsi` differs =
>   ASLR), C (`--in-process-gpu`) `PNG_APPEARED i=5` 3737 bytes; both arms emit ZERO bytes of
>   guest stderr. cb41 (parent warms only the first `warm` of 32 offsets, child reads all 32 vs
>   its own `pread`): `bad_cold=0/32` in every arm, gen 1 and 2, and `cb41.err` proves they are
>   real cross-process forks (`spawn_cross_process_fork_child elapsed_ms=33..55`,
>   `vmem-adopt-probe ... VERIFIED`). **Cold file pages are READ byte-exactly.**
> - **cb52 READ THE DYING GPU PROCESS'S LOG; cb49 IS THE FIRST MINIMAL REPRO.** cb52 arm Z
>   (default) vs F (`--no-zygote --no-sandbox`, PNG i=9 3779 bytes): Z's GPU pid stops after its
>   3rd `No entry found for gpu-process/*` and the run has **ZERO `GPU.*` histograms** (F has
>   `GPU.GPUProcessLaunchTime`, `GPU.HardwareAccelerationModeEnabled`,
>   `GPU.ProcessLifetimeEvents.SwiftShader`, ...) -- it dies before any GPU metric exists.
>   **Every forked child's timestamp is `0100/000000`** (an all-zero `tm`: `localtime_r` FAILED)
>   vs F's `1004/233303`. cb52 `.err`: `pid=47 Exception(14) rip=0x0 cr2=0x0 error_code=0x14
>   rax=0x0 rdx=0x1 rcx=0x14000 rdi=0x0` at t=0.147s, then WEDGED --
>   `rt_sigreturn: guest ucontext carries a zero rip or rsp, refusing the restore pid=47` at
>   t=37.5s, which is why a fatal fault still burns the whole 90s cap. cb54 REFUTED "a fresh
>   mapping made in a child is not backed" (`MATCH=True` parent/gen1/gen2).
> - **cb46/cb46b: THE DISCRIMINATOR IS THE ZYGOTE FORK, NOT THE SANDBOX.** `--no-sandbox` alone
>   is NO_PNG (cb46 E) but `--no-zygote --no-sandbox` PNGs (cb46b F i=24 3808 bytes; G, same
>   without vulkan, i=9; C `--in-process-gpu` i=8) -- zygote ON + sandbox OFF fails, zygote OFF +
>   sandbox OFF works, so **a GPU process made by CROSS-PROCESS FORK dies and one made by execve
>   paints** (`--no-zygote` alone is refused: `Zygote cannot be disabled if sandbox is enabled`).
>   With cb40 (`LD_BIND_NOW=1` dies at the IDENTICAL `rax=0x0` snapshot) the NULL is a function
>   pointer in WRITABLE data, not an unresolved PLT slot.
> - **REFUTED: write-sharing** (cb43 anon 4MiB / 64MiB scratch / the 324MB chromium are
>   `LIVE clean` AND `AFTER_EXIT clean`; cb47 adds HEAP 8MiB `clean` -- the heap had NEVER been
>   tested, and it is where ld.so's state lives). **Cold writes** (cb44 `wrote_B bad=0`).
>   **A lookup through an inherited `link_map`** (cb47/cb48 `dlsym` at gen 1 and gen 2 returns
>   the parent's exact addresses and `NSS_VersionCheck` runs). **The `mprotect` EACCES as the GPU
>   cause** (cb40's `.err` has ZERO `refusing mprotect`).

## 2. cb60-cb66: the cause, and the acceptance run

The bug: `get_layered_nodeinfo` (`litebox/src/fs/layered.rs:564-575`) stamped
`ino = node_info_lookup.len() + 1` on the first stat of a path, over a per-process map keyed by
the underlying layer's `NodeInfo`, with `dev` fixed at `DEVICE_ID = 0x4c797273`. A cross-process
fork child rebuilds its FsState from the parent's writable layer with an EMPTY map, so it handed
out 1, 2, 3, ... in touch order.

Why that is fatal: glibc's `_dl_map_object` has two "already loaded" exits -- (a) a name/soname
match over `l_name`/`l_libname`, and (b) an `(l_dev, l_ino)` match against the **`fstat` of the fd
`open_verify` just opened**. (b) is the one that fired. The `l_dev`/`l_ino` of every startup
object are the PARENT's numbers.

Trail:

- **cb60** -- a fork child's `dlopen` returns an object whose `l_name` is NOT the path it asked
  for. `dlinfo(handle, RTLD_DI_LINKMAP)` gives the `link_map*`; `l_addr` is at offset 0 and
  `l_name` at offset 8 on x86-64.
- **cb61** -- parent-vs-child `open()` content census over 79 libraries:
  `PATHS_WHOSE_OPENED_CONTENT_DIFFERS=0`. Refutes "the wrong file was delivered".
- **cb62** -- `fstat(fd)` vs `stat(path)`: `FSTAT_DISAGREES_WITH_OWN_STAT=0`,
  `FSTAT_IDENTITY_SAME_AS_PARENT=0 of 69`. Parent inos: libm=6, libz=8, libexpat=9, libc=10. So
  the numbers are self-consistent inside each process and simply differ across them -- which is
  exactly the shape that makes ld.so's (b) exit fire in a child and never in a parent.
- **cb63** -- `LD_DEBUG=libs` inside the fork child (it is initialised at startup and inherited
  across `fork`, so a child prints its own trace; ld.so writes to fd 2, so `dup2(w,2)` captures it
  through a pipe). The child takes the "already loaded" exit and prints no load lines.
  `0x7feffffb4xxx` addresses are the STARTUP objects (ld.so's minimal-malloc mmap region);
  dlopen'd objects get heap addresses (`0x1c...`). A byte copy of libEGL.so.1 at another path
  (`/tmp/cb63_egl_copy.so`) loads fine, so file content is irrelevant.
- **cb64** -- the smoking gun. Symlink vs copy: in one child libEGL.so.1 drew ino 2 and loaded
  fine, a byte-identical COPY of it drew ino 6 and `dlopen` returned the already-loaded libm
  (handle `0x7feffffb4000`, the very handle `dlopen("libm.so.6")` returns), and libX11-xcb.so.1
  drew ino 8 and came back as libz. ino 2 and 7 loaded fine. Pattern: **stat AFTER the dlopen** --
  the map caches, so `stat` returns the number ld.so was handed.
- **cb65** -- acceptance. All 10 child `dlopen`s name themselves; `IDENT_CHANGED_VS_PARENT=0 of
  69`; `TRUE_COLLISIONS_DISTINCT_FILES=0`. The dlopen sweep runs FIRST in a fresh child (that is
  when its counters would have been smallest) and the parent control LAST, because pre-loading a
  library in the parent turns the child's `dlopen` into a refcount bump and silences the bug.
- **cb66** -- the goal test. Arm Z = DEFAULT (zygote ON, chromium's OWN sandbox, GPU process
  forked from the zygote): `PNG_APPEARED i=6`, `HAS_PNG size=3876`,
  `CENSUS 800x600 depth=8 color=2 #20c0f0=99.95% blueish=99.99%`, `CHROMIUM_RC=0`. Arm C
  (`--in-process-gpu`, the old positive control): identical. A forked child's log now reads
  `1005/000939` instead of cb52's `0100/000000` -- the same breakage had been breaking
  `localtime_r`.

## 3. Regression

`reg1.sh` re-run after `f345821` (`.wfgy/reginode.*`): inetfork1/2, shmfork1/2/3, flockx1 all
PASS; `.err` 273841 bytes with **zero `panicked at`, zero `error_code=`, zero `Exception(14)`**.
One probe-side defect remains in `inetfork1.sh`: `[child] ABORTED TypeError('%d format: a real
number is required, not NoneType')` -- the `errno=%d`-on-`socket.timeout` trap that `AGENTS.md`
records as fixed is still present in that one arm.
