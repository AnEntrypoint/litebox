# 2026-10-04j -- recompact of AGENTS.md: prose moved out of the index

Companion to `AGENTS.md` (which is a CURRENT-STATE index; this holds the same facts in prose). Blocks
below were moved out of `AGENTS.md` verbatim during the 2026-10-04j recompact because they are
recipes, not rules.

## XWD grab: decode with the MASKS, never by byte position

Header `struct.unpack(">24I", raw[:96])`: `f[11]` = bytes_per_line, `f[3]` depth, `f[4]`/`f[5]` w/h,
`f[7]` byte_order, **`f[14]`/`f[15]`/`f[16]` = r/g/b masks** (`f[13]` is `visual_class` -- using
13/14/15 rotates every colour, which is how apps1 reported xterm's red as `0,0,255`). Pixel words are
little-endian when `byte_order == 0`, masks `00ff0000/0000ff00/000000ff`, so **in memory the bytes
are B,G,R**. Pixels start at `max(len(raw) - w*h*bpp, hsize + ncolors*12)`. **Every `blue=0.00%` from
chrD19 and earlier is INVALID.** **A GTK app's 10x10 leader window cannot be grabbed** (`xwd -id` ->
`BadMatch` `X_GetImage` on it -- apps1's "mousepad failed" was that artifact); grab the named
top-level window, or `-root` as ground truth.

## selkies' capture REQUIRES MIT-SHM at the X server

`run_shm_capture` HARD-FAILS on `shm_query_version` (`pixelflux/src/x11/mod.rs:1097-1138`) -- no
XGetImage fallback. The client then hits `ERROR:ws:FATAL: Initial reconfiguration completed, but video
pipeline did not start` (`websockets_mode.py:3936`); **the real diagnosis is the line above it**,
`Failed to start capture for 'primary': <e>` (`:5685`). After a FATAL `video_active` stays set.

## Apps: verified to paint (apps1/apps3 on `47cefb2`, zero panics)

Measured by xwd with the mask-aware decoder: `xterm` red `255,0,0` 97.4%, `xedit` white 94.2%,
`xcalc` 2 colours 83/17, `xeyes` 155 colours, `xclock` 188 colours, **`mousepad` (GTK3) white 87.4% +
grey246 7.2%**, **`thunar` (GTK3) 1855 colours white 60.9%**, chromium `--app` (xwd `blue=99.79%`,
CDP `99.78%`).
Absent from the image (untestable, not broken): `gedit`, `evince`, `pcmanfm`, `firefox-esr`,
`galculator`, `leafpad`, `import`/`convert`. Present, untested: `xfce4-terminal`.
Probes `.wfgy/apps1.sh` (rotated decoder) / `apps3.sh` (correct): no `timeout(1)`, no xfce4-session,
MIT-SHM ON, `/tmp/.X11-unix` 1777 before Xvfb, census in-guest.

## Two harness bounds that cost a run this pass

- **Concurrent boots are refused, not degraded.** `guest2.ps1` runs serialised by a boot lock
  (`target/release/.litebox-cache/boot.lock`, the lockfile's open handle): the second launch dies at
  once with `another ... boot is already in progress ... (lock held by pid N)` and writes a 0-byte
  `.out`, i.e. it looks like an instant crash and the arm never runs. cb4a was lost this way by
  launching cb9 and cb4 in parallel. "Never run two full-stack runs concurrently" is enforced, not
  advisory.
- **Budget forks, not wall time.** Every cross-process fork child rebuilds the OCI rootfs in its own
  heap, and the default image here is `webtop:debian-xfce` (~2.6 GB layer, ~87-124 adopted regions) --
  one child costs tens of seconds on cache HIT. cb9 does 18 forks (5 cases x 3 generations + 3
  descents) and produced exactly 2 of its lines before a 240 s cap killed it (`exit: -1`). Cheap
  probes with 12 forks (cb12/cb13) fit in 900 s; give a fork-heavy probe 1200-1500 s.
- `exit: -1` with a non-empty `.err` and a truncated `.out` means the harness cap, not a crash
  (`cb9a.err` had zero `panicked at`, zero `error_code=`).

## Why the carried-TCP rx fix stops where it does

The faithful design would be: for a fork-shared socket the tick never MOVES rx -- it only wakes the
waiters, and each reader pulls from the shared smoltcp socket inside its own `receive`. That is
exactly Linux (bytes leave the shared queue once, whoever reads first gets them) and it removes the
last race. It was rejected because `IOPollable::check_io_events` computes `is_readable()` from the
proxy's own rx buffer: with no drain, `poll`/`epoll` on a shared socket reports nothing readable, the
application never calls `read`, and the connection stalls. Making that faithful needs
`check_io_events` to pull through the `Network`/socket set, which it has no handle to. So the gate
stays "the tick drains into a proxy that has an observer", and the residual case -- BOTH referents
holding a live observer, e.g. a parent epoll set plus a child's blocking read -- still lets either
drain win. Unmeasured; not closed.

One shared `agentplug-runner spool` daemon per machine serves ALL registered projects, and it
self-recycles after 1h fully idle. If the MCP server process predates the supervisor/watchdog change
(`ee5855a`), nothing revives it: dispatches sit `queued_not_yet_claimed` until the timeout and
`mcp__gm__gm` returns `timed_out: true` with `daemon: alive=false`. Revive with one
`agentplug-runner spool` whose cwd is the project (a fresh daemon pid then serves both projects); a
dispatch that outlived its daemon is still retrievable with `resume_task`. Fixed in `c:\dev\gm`
(`9504d9b`, gm-mcp `8571615`): the poll loop re-asks for a runner on every wake, self-throttled, and
`runnerEnsureInFlight` stops ~50 contending runners stacking behind one cold start (~11s warm /
~100s cold).

## The GPU process's NULL is a failed LAZY PLT bind (cb4b), and what cb14/cb15 excluded

cb4b ran both headless arms and caught the fault in-run. Arm A (default, separate GPU process):

```
[A] GPU_PIDS ['197']
[A] GPU_PID_197_LINES 3
[A] gpu| [197:197:0100/000000.907470:VERBOSE1:base/allocator/scheduler_loop_quarantine_config.cc:198] No entry found for gpu-process/global.
[A] gpu| [197:197:0100/000000.908378:...] No entry found for gpu-process/*.
[A] gpu| [197:197:0100/000000.909495:...] No entry found for gpu-process/main.
[A] LIBFAIL| 197: /lib/x86_64-linux-gnu/libnssutil3.so: error: symbol lookup error: undefined symbol: vkGetInstanceProcAddr (fatal)
[A] LIBFAIL| 197: /lib/x86_64-linux-gnu/libnssutil3.so: error: symbol lookup error: undefined symbol: vkGetInstanceProcAddr (fatal)
[A] NO_PNG
```
and in `.wfgy/cb4b.err`: `diag-guest-exception: pre-signal snapshot pid=197 tid=197 comm=chromium
exception=Exception(14) kernel_mode=false rip=0x0 rsp=0x7fefffeea818 cr2=0x0 error_code=0x14
rax=0x0 rdx=0x1 rcx=0x14000 rsi=0xac11c03ff6 rdi=0x0`. So the old chrshot8 reading ("`call *rax`
from `dlsym` returning NULL") is refined: no dlopen is involved at all -- a PLT stub called
`vkGetInstanceProcAddr`, glibc's resolver returned 0 and the stub jumped there (`0x14` = instruction
fetch, not-present page). `rdi=0` is the NULL instance argument of the intended
`gpa(NULL,"vkCreateInstance")`.

The messages that name an object which cannot want that symbol are the real evidence, and they appear
in BOTH arms (so the browser hits them too and survives):

```
[B] LIBFAIL| 15:  /usr/lib/chromium/chromium: error: symbol lookup error: undefined symbol: localtime64 (fatal)
[B] LIBFAIL| 15:  /usr/lib/chromium/chromium: error: symbol lookup error: undefined symbol: localtime64_r (fatal)
[B] LIBFAIL| 15:  /usr/lib/chromium/././libvk_swiftshader.so: error: symbol lookup error: undefined symbol: wl_display_dispatch (fatal)
[B] LIBFAIL| 15:  /lib/x86_64-linux-gnu/libwayland-client.so.0: error: symbol lookup error: undefined symbol: wl_display_get_registry (fatal)
```
Chromium DEFINES `localtime64`/`localtime64_r` (cb14 dynsym dump: `localtime64:DEF
localtime64_r:DEF`), libvk_swiftshader.so exports exactly 4 symbols and imports none from wayland
(`wl_display_dispatch:absent`), and libwayland-client exports `wl_display_dispatch` itself. NSS and
vulkan are unrelated. So the objects named are right and the symbol names are not theirs -- the
loader is reading a symbol index/table that does not belong to the call site. Chromium's GPU-process
timestamps `0100/000000.907470` (month 01, day 00) are the same failure showing up as garbage time.

**Ruled out, with numbers (do not re-run these):**

- cb14: file content is faithful. `pread == mmap` for every object (`pread_eq_map=True`,
  `slice_bad=0/100`) over libnssutil3, libc, libwayland-client, libvulkan, libvk_swiftshader and the
  324MB chromium binary -- in the parent, after touching 200MB, and in two fork children.
- cb14: the symbols resolve. `LD_BIND_NOW=1 /usr/lib/chromium/chromium --version` -> rc=0
  (`Chromium 153.0.8010.52`), and `LD_BIND_NOW=1 ldd -r /usr/lib/chromium/chromium` reports no
  undefined symbol at all.
- cb15: fork keeps dirty private pages. Parent writes a 64-byte pattern at 24 offsets in
  `MAP_PRIVATE|PROT_READ|PROT_WRITE` mappings of libnssutil3 (211KB, under the lazy threshold),
  libvulkan (487KB) and libvk_swiftshader (5.4MB); a fork child that does NOT exec sees
  `ok=24 reverted_to_file=0 zeroed=0 other=0` for all three -- whether the parent had touched every
  page first (case A) or not (case B) -- and again after the child churns 64MB; anonymous control ok;
  the parent's own copy is intact afterwards; and `dlopen("libvulkan.so.1")` +
  `dlsym("vkGetInstanceProcAddr")` in the fork child returns `0x7fefe8e91590`.

What is left is therefore not "wrong bytes somewhere" but "a lazy bind in this one process reads the
wrong thing": the GPU process is the zygote's fork child (the browser process is the original, and it
survives), and it is the only process that both dlopens the vulkan stack and calls into it cold.
`cb16.sh` tests that split -- arm N runs arm A under `LD_BIND_NOW=1` (if the PNG appears, the lazy
path is the failure and every eager bind is fine) and arm T runs it under `LD_DEBUG=bindings` with
`LD_DEBUG_OUTPUT` to catch the last bindings of the dying pid.

Probe-writing notes that cost a run: `ctypes.RTLD_LAZY` does not exist in this image's python
(use `os.RTLD_LAZY`); a python traceback goes to the guest STDERR, so a script that dies mid-way
shows up as a bare `rc=1` in `.out` with the reason only in `.err`.

## cb17 / cb18 -- who fails a lookup, and who dies of it

**Harness shape that works.** `LD_DEBUG` writes to STDERR, and STDERR is an inherited pipe, so the
launching shell sees EVERY process's lines in one stream (`LD_DEBUG_OUTPUT` is useless: a fork
child's file is visible only to the shell that reaps it -- cb16). Capture with a python filter on
that pipe, attribute each line to a pid by the leading `^\s*(\d+):` (LD_DEBUG) or `^\[(\d+):`
(chromium log), keep a per-pid `deque` ring, and print the ring of the dying pid.

**cb17** (`LD_DEBUG=bindings`, default GPU process, one arm): `CAPTURED_LINES 91828`,
`PID_HIST [('14',24063),('1',21693),('54',21444),('26',20992),('61',1288),('?',1205),('17',549),('19',549),('71',13),('88',7),('51',5),('118',5),('154',5),('55',4)]`.
Pid 51 = the GPU process: its whole ring is 3x `No entry found for gpu-process/*` plus 2x
`libnssutil3.so: error: symbol lookup error: undefined symbol: vkGetInstanceProcAddr (fatal)`.
**It emits NO binding line at all.** `LD_DEBUG=bindings` prints only on a SUCCESSFUL `do_lookup_x`,
so the GPU process dies on its first lazy bind. cb17 lost the rest of the evidence: `hits` was
capped at 4000 but only `hits[:200]` was printed.

**cb18** arm A (same, with `cap4.py` printing every fatal line at once plus the 25 preceding lines
in ANY process): `CAPTURED_LINES 91826 FATALS 15`,
`FATAL_PER_PID [('14',6),('53',3),('26',2),('1',2),('50',2)]`. The fatals are
`localtime64`, `localtime64_r`, `nspr_use_zone_allocator` (chromium), `C_GetInterface`
(`libnssckbi.so`), `g_module_unload` and `g_io_dconfsettings_load` (`libdconfsettings.so`), all
right after a `calling init: <that object>` -- i.e. optional dlsyms whose NULL the caller handles.
Pid 50 is the second GPU pid (the retry after the first died) with 2x `vkGetInstanceProcAddr`.
For three fatals the "object name" glibc printed was the full chromium command line, and the GPU
process's chromium timestamps are garbage (`0100/000000`).

**Conclusion.** `symbol lookup error ... (fatal)` is glibc's wording for a failed `dlsym`-class
resolution and is survivable -- the browser process (pid 14, generation 0, never forked) collects 6
of them and runs for minutes. The GPU process is the only one that CALLS its NULL: `rip=0x0`,
`error_code=0x14` (instruction fetch from a non-present page), `rax=0x0`. So "who fails a lookup"
is the wrong question; the question is why the GPU process's resolution of `vkGetInstanceProcAddr`
returns 0 while the same resolution in a python fork child returns `0x7fefe8e91590` (cb15).

**cb4b's pid-197 trail, re-read against that.** 24 syscalls: `newfstatat(<full GPU cmdline>)`,
`readlink("/proc/self/exe")`, `openat` -> `Some("")`, `read(fd=0xf, 0x340)` (an ELF header),
`fstat`, `close`, `getpid`, `writev(2)` = error #1, `gettid`, five `mprotect(prot=0x3)`
(0xc4000cc000+0x4000 ... 0xc4000f0000+0x8000), `gettid` x3, `newfstatat`, `readlink`,
`openat(0xc4000074b0)` with **no read/fstat/close = it FAILED**, `newfstatat`, `readlink`,
`getpid`, `writev(2)` = error #2, then the NULL call. Two dlopen-shaped attempts, the second one
opening nothing -> the GPU process is trying to load a library it cannot open.

**cb18 arms B/C were lost**, and the cause is worth keeping: chromium does NOT exit after a failed
`--screenshot`, so `wait "$ARM"` blocked until the harness `-Secs` cap and the later arms never
ran. Arms must be bounded by killing (`kill -9 $ARM; pkill -9 -x chromium; pkill -9 -f cap5.py`),
and the capture's own deadline must be SHORTER than the PNG poll window or the capture's summary
prints after the arm is already dead.

**cb21** (running) is the sandbox hypothesis: the GPU process is the only chromium process under
chromium's OWN sandbox. `LD_DEBUG=libs` prints `find library=`, `trying file=`,
`cannot open shared object file` and `calling init:` per process, so it names what each process
loaded and what it could not, at a fraction of `bindings`' volume. Arms L1 default,
L2 `--disable-gpu-sandbox`, L3 `--disable-seccomp-filter-sandbox`, L4 `--no-zygote` (the GPU
process is normally the zygote's fork child, generation 2; `--no-zygote` makes it generation 1,
which separates fork depth from "is the GPU process").

## cb21 arm L1 -- the GPU process loads NOTHING

cb21 was reaped by HOST memory pressure after arm L1 (L2-L4 unrun). L1's answer is unambiguous:

```
[L] CAPTURED_LINES 5090
[L] PID_HIST [('61',1288),('1',1022),('14',1010),('54',759),('26',716),('?',183),('17',37),('19',37),('81',13),('90',7),('51',5),('120',5),('55',4),('107',4)]
[L] CALLING_INIT_PER_PID [('14',109),('26',101),('1',101),('54',101),('17',5),('19',5)]
[L] GPU_LOG_PIDS ['51']
[L1] NO_PNG
```

and pid 51's whole ring is five lines:

```
[51:51:0100/000000.660556:VERBOSE1:base/allocator/scheduler_loop_quarantine_config.cc:198] No entry found for gpu-process/global.
[51:51:0100/000000.661115:...] No entry found for gpu-process/*.
[51:51:0100/000000.662290:...] No entry found for gpu-process/main.
        51:	/lib/x86_64-linux-gnu/libnssutil3.so: error: symbol lookup error: undefined symbol: vkGetInstanceProcAddr (fatal)
        51:	/lib/x86_64-linux-gnu/libnssutil3.so: error: symbol lookup error: undefined symbol: vkGetInstanceProcAddr (fatal)
```

**The GPU process has ZERO `calling init:` lines.** It does not dlopen anything, so there is
nothing for a sandbox to block: no `find library=libvulkan.so.1`, no `calling init: libvulkan.so.1`
(compare cb4b arm B, `--in-process-gpu`, where `calling init: /usr/lib/chromium/libvulkan.so.1`
does appear and a PNG follows). **The sandbox hypothesis is dead.** Also note the GPU log
timestamps are `0100/000000` -- chromium's clock read gives garbage in that process only.

### The empty path, and why it is real

cb4b's pid-197 trail decodes its first `openat`'s argument as `Some("")`. That decode is
`UserPtr::to_cstring` (`litebox_common_linux/src/user_pointers.rs:110`), which is
`RawConstPointer::to_cstring` (`litebox/src/platform/mod.rs:673`): a byte-at-a-time
`read_at_offset` loop that stops at the first NUL. `read_at_offset`
(`litebox/src/platform/common_providers/userspace_pointers.rs:150`) is

```rust
let src = ptr.wrapping_add(usize::try_from(count).ok()?);
let src = V::validate(src.cast_mut())?.cast_const();
```

and the ONLY `ValidateAccess` impl in the tree is `NoValidation` (`codesearch literal
"ValidateAccess for"` -> one hit, `userspace_pointers.rs:81`), i.e. `validate` is the identity.
So a failed read yields `None` (printed `None`), and `Some("")` can only mean **the first byte at
that guest address read as 0x00**. The GPU process handed `openat` an EMPTY path. (For syscalls
where arg1 is not a pointer at all this decode prints whatever is there -- the same trail's
`newfstatat(<full GPU cmdline>)` is arg1 happening to point into the argv area.)

### The lead that follows

A lazy file map chunk is armed `PAGE_NOACCESS` and filled by the VEH from a `'static` source slice
(`litebox_platform_windows_userland/src/lazy_file_map.rs`: `register()` at 517 arms NOACCESS for
>= `MIN_LAZY_LEN` = 256 KiB; `permission_update_ranges` at 576 re-protects). **If a guest
`mprotect` over such a range makes it readable WITHOUT filling it, every later read returns ZEROS,
no fault is taken, and the fill never happens.** ld.so maps a library `PROT_NONE` and then
`mprotect`s each PT_LOAD -- exactly the five `mprotect(prot=0x3)` calls in the pid-197 trail -- and
chromium's `.rodata` is a 324 MB file mapping, i.e. lazy. Zeroed rodata => an empty dlopen path =>
NULL => `call *rax` with `rax=0` => `Exception(14) rip=0x0 cr2=0x0 error_code=0x14`.

`.wfgy/cb22.sh` tests this with no chromium and no LD_DEBUG: cases A-E compare 64 bytes at six
offsets against `os.pread` for `mmap(PROT_READ)` untouched, `mmap(PROT_READ)`+`mprotect(PROT_READ)`,
`mmap(R|X)`+`mprotect(R)`, `mmap(PROT_NONE)`+`mprotect(R)`, and a parent mapping that a FORK CHILD
`mprotect`s to `R|X` (the zygote/GPU shape); case F hands `openat` a path string that lives inside
a 512 KiB (lazy) file mapping, with an anon-page control and a <256 KiB file control.
