# ADVISORY-001: Fundamentals for XFCE on litebox (Windows first)

Written by the advisor session (`advisor-db`) for the agent working in `C:\dev\litebox-main`.
Everything here is derived from the code in the tree (drm.rs, presentation.rs, lib.rs,
fork_verify.rs, process_fork.rs, the rewriter, the probes) plus outside references, not from
AGENTS.md or memories. Where I could not verify something I say so. Push back with evidence.

## 0. Where the project actually is

- The only real pixels ever presented are weston-desktop-shell's own panel (clock + icon).
  No XFCE component has painted anything. "Almost there" is not an accurate description of
  the display path; it is accurate for the process-emulation path (everything loads and
  runs for a while).
- The pass-N loop has been finding one emulator gap per full-stack run. That is the slowest
  possible discovery rate. Section 3 replaces it with tooling that finds whole classes at once.
- Three structural Windows-platform problems generate most of the "Heisenbugs", the
  minutes-long startup, and the crash triage churn. They are fixable, and fixing them removes
  hundreds of lines of reactive machinery rather than adding more.

## 1. Structural problems on the Windows platform

### 1.1 FS-base TLS: stop depending on `wrfsbase` at all

Evidence: `pass_dumpframes_run2.log` shows consecutive `EXCEPTION_SINGLE_STEP` traps on one
thread, microseconds apart, with `rdfsbase` alternating `0` and `0x23ceb28`. Measured on this
host with `advisor/probes/fsbase_probe.c` (2026-09-03): a user-written FS base survives plain
syscalls, `SwitchToThread`, and VEH-handled exceptions, but is cleared to 0 by every real
context switch (a `Sleep(20)`, or a long user-mode loop that gets preempted). So every
blocking wait that deschedules a guest thread, and every timer preemption under load, is
followed by a faulting `fs:` access and a VEH repair round trip (5-20 us), and the repair is a
race by construction (a `fs:` access whose effective address with base 0 happens to be mapped
reads the wrong memory silently). Under a multi-process desktop that is thousands of resets
per second.

Fundamental fix: virtualize the thread pointer in the rewriter, exactly as this repo already
does for AArch64 (`litebox_syscall_rewriter/src/arm64.rs` rewrites `MRS/MSR TPIDR_EL0` to a
host-managed slot). For x86-64:

- Keep the guest FS base in a per-thread host slot: a `TlsAlloc` index read as
  `gs:[0x1480 + 8*idx]` (TEB.TlsSlots, stable offset used by Go and others), or a dedicated
  field reachable from `gs:`.
- Rewrite every instruction with a `0x64` (FS) segment override. iced-x86 (already a
  dependency) gives you the operand shape. Dominant musl forms: `mov r64, fs:[disp32]`
  (`fs:0` self pointer, `fs:0x28` canary) and `mov r64, fs:[-off]` (initial-exec TLS).
  Generic scheme for any shape: pick a GPR the instruction does not use, spill it to a second
  TLS slot, load the FS base into it, re-encode the instruction with `[reg+orig_mem]`, restore.
  Trampolines are already how the rewriter handles size growth for `syscall`.
- `arch_prctl(ARCH_SET_FS)`, `clone(CLONE_SETTLS)`, `execve` write the slot. `rdfsbase`/
  `wrfsbase` in guest code (rare; musl does not use them) get rewritten to slot accesses.
- Runtime patching already exists for unpatched binaries (`mm.rs` `patch_code_segment`), so
  the same path covers ld.so, libc and every .so without a separate offline step.

Result: no FS-base repair loop, no `is_verifying`/`rdfsbase()==0` heuristics in the VEH, no
per-syscall exception, deterministic behaviour. Measure before/after: exceptions per second
and syscalls per second during xfce4-session startup.

### 1.2 fork(): the in-process relocated copy cannot be made correct

Current design (fork_verify.rs, 2519 lines; process_fork.rs, 3240 lines; ctxwatch.rs): the
child runs as a thread in the same host process on a copy of memory at different addresses,
and stale pointers are "healed" reactively while single-stepping. This is unsound by
construction: heap-internal pointers, function pointers in data, pointers inside atomics and
lock words, pointers stored as integers or XOR-encoded, and anything the child computes from
a stale value before it is dereferenced. Single-stepping is also 10^3-10^4x slower than
native, which is where much of the "pretty long wait" goes (measure it: total time with TF set).

Real Linux and macOS give the child identical addresses for free. Windows can too:

**Recommended design: one host process per guest process, cloned at identical addresses,
with litebox kernel state living in a process-shared section.**

1. Clone with `RtlCloneUserProcess` (ntdll wrapper over `NtCreateUserProcess` in clone mode;
   the child's thread resumes at the same point and receives `STATUS_PROCESS_CLONED`). The
   kernel copies the address space copy-on-write at identical addresses; mapped section
   views stay shared (`ViewShare`); inheritable handles are copied with the same values; only
   the calling thread exists in the child, which matches Linux fork semantics exactly.
   Reference: huntandhackett "The Definitive Guide To Process Cloning on Windows"
   (https://github.com/huntandhackett/process-cloning). Midipix ships a copy-on-write fork on
   NT-native cloning, so this is a proven approach for real POSIX workloads.
2. What the child must not do: load DLLs, touch CSRSS-backed APIs (console via kernel32,
   user32/GDI, COM, activation contexts). NT syscalls, `NtCreateThreadEx`, heap, section
   mapping, `NtWriteFile` on inherited handles are fine. Consequences for litebox: the
   guest+kernel process must contain no winit/wgpu/COM state (see 1.3), must write logs via
   file/pipe handles not `WriteConsole`, and must preload every DLL it will ever need at
   startup. Test `CreateThread` in a clone once; if it misbehaves, use `NtCreateThreadEx`.
3. Kernel state shared across processes. Litebox's `#[global_allocator]` is already a custom
   allocator (`SafeZoneAllocator`) over platform pages, and locks come from the platform's
   `RawMutex`. Back the kernel allocator with a pagefile-backed section mapped at one fixed
   address in every guest process (the clone inherits the view), so every kernel pointer is
   valid in every process. Make `RawMutex` and futex cross-process: spin, then park on a
   per-thread auto-reset kernel event recorded in a shared waiter list keyed by address;
   the waker pops waiters and `SetEvent`s them through a handle duplicated (or opened by
   name) into its own process, cached per (process, waiter). `WaitOnAddress` is per-process,
   and keyed events match waiters only within one process (verified by
   `advisor/probes/clone_probe.c`: both sides time out), so neither can be the primitive.
   Guest MAP_SHARED, memfd
   and dumb buffers are already section-backed and stay shared; guest private memory is
   CoW per process. Objects that embed host HANDLEs must instead carry an object id and
   resolve to a local handle lazily via `DuplicateHandle` from the owning process (clones share
   the token, so this is permitted).
4. Cross-process task control. Signals to a thread in another process: open the thread by
   id and use `QueueUserAPC2` with `QUEUE_USER_APC_FLAGS_SPECIAL_USER_APC`. Verified with
   `advisor/probes/apc_probe.c`: it interrupts a cloned child's thread that is spinning in
   pure user mode, with no suspend/inspect/resume dance. The APC dispatcher resumes through
   `NtContinue` with the interrupted CONTEXT, so the guest signal frame can be built there
   (Wine's `KiUserApcDispatcher` handling is a reference). The same primitive should replace
   today's intra-process `SuspendThread`/`SetThreadContext` interrupt path, which lib.rs
   itself documents as non-atomic. Exit and wait4 go through shared task state plus the
   per-waiter event wake from item 3.
5. execve stays in-process (tear down guest mappings, load the new image in the same host
   process). vfork/`posix_spawn` (`CLONE_VM|CLONE_VFORK`) can additionally take a cheap path:
   run the child on a new thread sharing memory with the parent thread parked until exec or
   exit, which is exactly what Linux does.
6. Delete after landing: fork_verify.rs, ctxwatch.rs, the copy experiments in process_fork.rs,
   `litebox::mm::AddressRelocations` and `ForkChildVerificationProvider` (44 references across
   core, shim and runner). The Linux and macOS platforms never needed any of it.

Staging that keeps every step verifiable on its own: (a) shared-section allocator plus
cross-process `RawMutex` with a single process (no behaviour change, tests pass); (b) a 50-line
probe that clones the runner with `RtlCloneUserProcess`, has the child run guest code and exit;
(c) real fork with fd-table copy and pid bookkeeping; (d) signals, wait4, SCM_RIGHTS across
processes; (e) run the fork/futex/signal conformance corpus (3.7) 200x.

Do not build the vfork-only shortcut as the general fork: daemonizing programs in the XFCE
stack (`dbus-daemon --fork`, xfsettingsd, Thunar --daemon) fork without exec, and the
"parent parked until child exits" semantic deadlocks them. That road leads to per-program
workarounds, which is what the user has asked you to avoid.

### 1.3 Presentation: the display must not live inside the guest process

Today `Presenter` (winit + wgpu) runs as a thread in the same host process as the guest and
the shim, `LITEBOX_DUMP_FRAMES` is implemented inside the presenter (so headless runs cannot
observe frames at all), and the pass-308/309 crash is a race between wgpu setup and DRM
startup in one process. All three go away with one change:

- The DRM device owns the scanout: on `PAGE_FLIP`/`SETCRTC` it copies the framebuffer into a
  named, shared "scanout" section (double-buffered, with a sequence number and a signalled
  event). This is the vkms writeback-connector idea: the kernel side always has the current
  frame regardless of any window.
- `litebox-presenter.exe` is a separate process that maps the scanout section and blits it
  with wgpu. It can be started at launch (`--gui`), started later, killed, hidden, shown;
  the guest never notices. Input goes back through a small ring in the same section or a
  pipe into the evdev queue.
- Headless verification reads the section: `LITEBOX_DUMP_FRAMES=1` writes BMP/PNG on the
  kernel side, and a control channel (named pipe `\\.\pipe\litebox-<id>`) answers
  `screenshot`, `show`, `hide`, `key`, `mouse`, `ps`, `strace on|off <pid>`, `kill`. This is
  what subagents use to verify runs concurrently without touching the screen.
- This also makes the guest process clone-safe (1.2) and makes the macOS/Linux presenters
  symmetrical (they need their run loop on the main thread of some process anyway).

### 1.4 Shared memory: memfd, /dev/shm and MAP_SHARED files need a page cache

Verified in the tree and in the frame dumps (2026-09-03):

- `mm.rs` `try_memfd_mmap` keeps two stores for a memfd (the in-mem file's `Vec<u8>` and a
  host shared-memory handle) and, on every `mmap` by any mapper, copies the Vec (zeros after
  `ftruncate`) into the shared object. Whatever a client drew through its own mapping before
  the compositor mapped the pool is erased. `file.rs` `resize_memfd_shared_backing` replaces
  the handle on any size change, orphaning existing mappings (libwayland-cursor's pool resize
  does exactly this).
- `mm.rs` ~687-698 rejects `MAP_SHARED|PROT_WRITE` on any regular file; there is no SysV shm
  (`shmget`/`shmat`); in-mem files are a flat `Cow<[u8]>`.
- Frame dumps: with `background-color=0xff002244` configured, the composited frame contains
  zero blue pixels; only the every-second clock and the launcher icon survive. Kiosk-shell's
  compositor-painted background and weston-simple-shm's every-frame redraw are unaffected,
  which is exactly the pattern a mmap-time wipe predicts.

Fix, staged:
1. memfd: the shared object is the only store from `memfd_create` on. `read`/`write`/`pread`/
   `pwrite`/`lseek`/`fstat`/`ftruncate`/`fallocate` operate on it through a kernel-side view.
   Create it as a pagefile-backed `SEC_RESERVE` section with a large maximum and commit pages
   as the file grows (`VirtualAlloc(MEM_COMMIT)` on a view commits for every view of the
   section); shrink by decommit. Identity then survives dup/SCM_RIGHTS/fork by construction
   and the sync code is deleted. Seals (`F_ADD_SEALS`) keep their current accept-and-record
   behaviour.
2. tmpfs page cache: `/dev/shm`, `/tmp`, `/run` (and ideally every in-mem file) store data in
   section-backed pages; `MAP_SHARED` maps the pages directly, `MAP_PRIVATE` maps them with
   `FILE_MAP_COPY` (copy-on-write for free, which also serves ELF loading), `read`/`write` copy
   from/to the pages. This unblocks the XDG_RUNTIME_DIR/`/tmp` fallbacks in
   `os_create_anonymous_file`, GLib mapped files, sqlite, and X11's shm paths.
3. SysV shm (`shmget`/`shmat`/`shmdt`/`shmctl`) on top of (2), for MIT-SHM under Xorg
   (GTK under X11 uses XShm via `shmget` by default).

mremap on shared mappings (`litebox/src/mm/linux.rs` `resize_mapping` ~1513-1560 and
`move_mappings` ~1600-1660): an in-place grow of a `shared_handle` VMA extends it with private
anonymous pages that are not part of the section (silent zeros on the other side), a move maps
the same handle at the new size and fails if the object is smaller, and file-backed mappings
hit `unimplemented!()` (host abort on a guest syscall). libwayland-server grows every wl_shm
pool with `mremap(MREMAP_MAYMOVE)` and disconnects the client on failure, so this is the
likely cause of the registry-time client drop (the first resize is libwayland-cursor's theme
pool right after `wl_seat`). With one large `SEC_RESERVE` section per memfd, in-place grow
maps the next section pages at `cur_range.end` and a move maps a larger view elsewhere.

Conformance probe for the corpus: memfd, ftruncate, mmap MAP_SHARED, write a pattern through
the mapping, pass the fd over a socketpair to a thread that mmaps and checks it; then grow
the file, mremap on both sides, write a pattern into the tail on one side and check it on the
other; finally pread the fd and compare with the mapping.

### 1.5a A shared VMA's range is not the view base (root cause of the recurring UnmapError)

Found 2026-09-03 after the logical-unmap fix landed. `VmArea` records only `shared_handle`
(mm/linux.rs:327); the address `MapViewOfFile`/`map_shared_memory` actually returned is never
stored. `vmas` is a `rangemap::RangeMap` (linux.rs:11,567) whose `remove()` splits and shrinks
entries. So:

1. A view maps at base `B` covering `B..B+N`; the VMA range coincidentally equals `B`.
2. A guest `munmap` of a subrange takes the new LOGICAL branch (permissions revoked, real view
   left mapped), then falls through to `self.vmas.remove(range)` (linux.rs:820).
3. If the removed subrange touched the FRONT, the surviving VMA is `B+k..B+N` while the real
   view is still based at `B`.
4. A later `munmap` of the rest passes the full-coverage test against the SHRUNKEN range, takes
   the physical branch, and calls `unmap_shared_memory(B+k..)`.
5. `UnmapViewOfFileEx` demands the exact original base, so it returns `ERROR_INVALID_ADDRESS`
   (0x1e7) -> `SharedMemoryError::Unaligned` -> `UnmapError(Unaligned)` -> panic at
   process.rs:4323.

Note the fix in 1.5 is what made step 4 reachable: previously step 2 panicked first. The fix is
correct and necessary; it exposed the next layer of the same missing invariant.

Fix: store the real `view_base`/`view_len` in the `VmArea` when a shared view is mapped and
propagate it unchanged through every RangeMap split. Physically unmap only when the removal
covers the WHOLE VIEW, and always pass `view_base` to `unmap_shared_memory`; everything else
stays logical. This also fixes the mirror-image leak (fragments unmapped back-to-front never
look like "full coverage", so the view is never released). Separately,
`unmap_shared_memory` should surface the real OS error instead of collapsing everything to
`Unaligned`, and process.rs:4323 should not panic on a guest-reachable failure (item 3.10).

### 1.5 Partial unmap of shared views panics the host

`litebox_platform_windows_userland/src/lib.rs` `unmap_shared_memory` calls
`UnmapViewOfFileEx(range.start)` and reports any failure as `SharedMemoryError::Unaligned`,
which the core turns into a panic (the xfwm4 "UnmapError(Unaligned)"). Windows can only
unmap a whole view by its base; Linux allows `munmap` of any page subrange. Fix: treat a
partial unmap of a shared view as logical (VirtualProtect the subrange to `PAGE_NOACCESS`,
record it in the VMA, physically unmap when the last page goes), never map an OS failure to
`Unaligned`, and longer term use placeholders (`MEM_RESERVE_PLACEHOLDER`,
`MEM_REPLACE_PLACEHOLDER`, `MEM_PRESERVE_PLACEHOLDER`) so views can be split and re-mapped at
exact addresses, which the fork clone and in-place mremap growth also want.

## 2. Which display stack can show a whole XFCE desktop

Facts: XFCE is X11-native; xfwm4 is X11-only. XFCE 4.20 Wayland mode requires wlroots
protocols (wlr-layer-shell for the panel and xfdesktop, foreign-toplevel for the taskbar) and
a third-party WM such as labwc or wayfire. Weston implements none of the wlr protocols; the
XFCE wiki says panel functionality there is "very limited" and the panel itself warns
"Wayland detected without layer-shell support". So xfce4-panel "never creating a surface"
under weston is expected, and the registry-roundtrip hypothesis is chasing the wrong thing.
Check `WAYLAND_DEBUG=1 xfce4-panel` for `zwlr_layer_shell_v1` in the globals list to close it.

Two stacks that can succeed, to run as parallel tracks with the same success metric (a
headless frame in which xfdesktop's background and xfce4-panel are both visible):

(a) Xorg on the virtual KMS device. `Xorg -config xorg.conf -novtswitch -sharevts -noreset :1`
with `Section "Device" Driver "modesetting" Option "kmsdev" "/dev/dri/card0"`, ShadowFB on,
glamor not built or disabled, `AutoAddDevices off`, an explicit `InputDevice` on
`/dev/input/event0` (xf86-input-evdev or libinput path backend), then `startxfce4`. Stock
XFCE, xfwm4 included, nothing experimental. The ioctls modesetting issues on this path,
which drm.rs must answer correctly (implement or fail with the errno the real kernel would):
legacy `DRM_IOCTL_MODE_ADDFB` (drm.rs only has ADDFB2), `GETCRTC`, `GETENCODER`,
`GETCONNECTOR` with a mode list, `GETPROPBLOB`, `DIRTYFB` (may return ENOSYS), `CURSOR` and
`CURSOR2` (must fail cleanly so the server falls back to a software cursor), `SETGAMMA`/
`GETGAMMA`, `WAIT_VBLANK`, `GET_CAP` for cursor size and `DRM_CAP_DUMB_PREFER_SHADOW`, and
`SETCRTC` with a real mode. Have a subagent enumerate them from xserver 21 / modesetting
source rather than trusting this list. Xorg also opens `/dev/tty0` and issues VT ioctls; the
seatd path in file.rs already emulates most of them.

(b) Keep weston and run Xwayland rootful and fullscreen (`Xwayland :1 -fullscreen
-geometry 1920x1080`, no `-rootless`), then `startxfce4` with DISPLAY=:1. Rootful Xwayland
gives xfwm4 and xfdesktop a real root window. Reuses everything already working; the Xwayland
software-path fault (page-aligned address, see 3.5) has to be fixed either way.

Drop native-Wayland XFCE under weston. If a Wayland-native desktop is wanted later, that is
labwc with the pixman renderer and the DRM dumb-buffer allocator, a separate project.

## 3. Diagnostics that remove guesswork (build these before more full-stack runs)

3.1 `LITEBOX_STRACE=1`: per-syscall line with timestamp since t0, pid/tid/comm, decoded args,
    result with errno name. `LITEBOX_STRACE_SUMMARY=1`: at exit, per-syscall count, total and
    max latency, error histogram, and a dedicated section listing every syscall, ioctl
    request, fcntl/prctl/setsockopt sub-command that returned ENOSYS/EINVAL/ENOTSUP/
    EOPNOTSUPP/EPERM with first caller comm+pid. One run of the stack then lists every gap.
    dbus-daemon already tells you inotify is ENOSYS; you should not need dbus to tell you.
3.2 `LITEBOX_CAPTURE_PROC_IO=<dir>`: each guest process's stdout/stderr teed to
    `<pid>-<comm>.log` so `G_MESSAGES_DEBUG=all`, `WAYLAND_DEBUG=1`, `xfce4-session --verbose`
    output is attributable.
3.3 Headless frames from the kernel side (1.3), plus PNG output and `screenshot` on demand.
3.4 Process timeline: exec of every binary, every exit with status or signal, all timestamped,
    plus a process tree on exit. Time-to-first-non-black-frame is the one number every run
    reports.
3.5 Guest fault report on every SIGSEGV/SIGBUS/SIGILL: rip, fault address, registers, the VMA
    list around the fault address with the creating syscall of the nearest mapping (fd,
    offset, size, mremap history), and module+offset symbolization from the ELF you loaded.
    A fault exactly at a page boundary in an SSE copy loop means "buffer shorter than the
    caller believes": mremap growth (musl realloc uses mremap for large blocks; `test_mremap`
    already flakes) or memfd ftruncate/mmap size mismatch for wl_shm pools.
3.6 Guest-side ground truth to localize lost pixels: `xwd -root` under Xorg,
    `weston-screenshooter` under weston, compared with the kernel-side dump and the
    presenter's frame. Three points on the pipeline tell you which stage dropped the pixels.
3.7 Conformance corpus in dev_tests (C, static, run 100x in CI): fork+exec, vfork,
    posix_spawn, daemonize (double fork, setsid), pipes across fork, unix socket SCM_RIGHTS
    and SO_PEERCRED (dbus EXTERNAL auth needs it), memfd+ftruncate+mmap+mremap, inotify,
    timerfd/signalfd/eventfd, nested epoll, futex wake across threads, munmap with unaligned
    length, mmap of /dev/dri dumb buffers. Consider building LTP's syscalls suite in the
    Alpine rootfs for a broad sweep.
3.8 Bisection by substitution: run each XFCE component alone under Xvfb inside litebox. If
    xfce4-panel misbehaves under Xvfb, the DRM/wgpu path is innocent and you have isolated an
    emulation bug without any display in the loop.
3.10 Panic sweep: every `unimplemented!`, `todo!`, `panic!`, `expect(`/`unwrap(` reachable
    from a guest syscall is a host abort triggered by guest behaviour. Grep them (about 30 in
    the shim, plus the mm layer's file-backed mremap arms) and convert each to the errno the
    real kernel returns, with an error-level log line naming the syscall and arguments.
3.9 A control-plane ground truth: run the same rootfs and recipe with
    `litebox_runner_linux_userland` on a real Linux CI runner (not WSL). Anything that fails
    there is a shim bug; anything that fails only on Windows is a platform bug. This split
    alone would have avoided several passes.

## 3B. PROVEN 2026-09-03: every XFCE run so far raced the X server's startup

Execve timeline extracted from `%TEMP%\postfix_fixed_1.log` (`DIAG_TIMELINE execve` lines):

    1.454  seatd
    4.461  dbus-daemon
    5.772  weston
    6.364  weston-desktop-shell
    9.883  Xwayland
    9.886  xfwm4            <-- 3 ms after Xwayland's execve
   16.322  xfce4-session

xfwm4 was started 3 milliseconds after Xwayland began executing. At that moment Xwayland has
created no X socket and loaded no libraries; its own dlopen chain does not finish until
~t=23.6. xfwm4 therefore failed with "cannot open display :1" and exited, and xfce4-session
later started against the same not-ready display. **XFCE's behaviour under litebox is
essentially unmeasured**: the deadlock trace, the "xkbcomp hang" and the various timeouts were
all taken against a display that did not exist yet or was still initialising. This does not
invalidate the real bugs found (memfd wipe, view_base drift, fork_verify map leak), but it does
mean the XFCE-specific conclusions rest on invalid runs.

Fix in the launch script, replacing any fixed sleep before starting X clients (Xwayland needs
~14 s to become ready here, so short fixed sleeps also explain some "non-determinism"):

    for i in $(seq 1 120); do xdpyinfo -display :1 >/dev/null 2>&1 && break; sleep 0.5; done

Socket existence (`/tmp/.X11-unix/X1`) is a weaker check, since the socket appears before the
server accepts connections. Same principle applies to every dependency in the chain: wait for
readiness, never for a duration.

## 3E. FASTEST OPEN BUG (2026-09-03): weston dies at t≈5.2 s, 100% reproducible

Best repro of the session: weston alone, no XFCE, no Xwayland needed, ~5 s, 3/3 identical.
weston (pid 7) takes SIGSEGV at `cr2=0x11ac0000`, `error_code=0x6` (user-mode WRITE to a
protected/non-present page), in a range flagged `VM_SHARED`. The cleanup pass afterwards fails
a `VirtualProtect` with ERROR_INVALID_HANDLE (6) and the runner PANICS at lib.rs:4990 (an
`assert!` inside `process_memory_range_by_regions` — itself a bug per item 3.10: one unexpected
OS return code aborts the whole session).

Decisive state data: the failing region reports `mbi_type=MEM_MAPPED` (a real section VIEW),
`mbi_state=MEM_DECOMMIT` (no backing at all) and `mbi_protect=0`. So a shared view is being
torn down while a live guest still has it mapped and is using it.

TESTED AND RULED OUT (2026-09-03, end of session): the `vma_range` clamp fix below WAS applied
and the crash reproduces identically (t=5.28 s, same signature). The clamp bug was real and the
fix is correct and kept, but it is NOT the cause of this crash. Likewise the `sys_mprotect`
bitmask fix: applied, crash unchanged. So the remaining cause is something that DECOMMITS the
view — which neither a protection change nor an over-broad `PAGE_NOACCESS` can do. Start the
next session by identifying what creates and what decommits the 8.29 MB `VM_SHARED` range at
guest address 0x11ac0000: it is confirmed NOT weston's executable (`main_base=0x101c0000`) and
has NO `diag-exec-mmap` entry, so it is not created through the normal execve/ELF-load path.

MOST SPECIFIC CANDIDATE (2026-09-03, end of session): `protect_mapping` applies permission
changes to WHOLE VMAs instead of the requested range. litebox/src/mm/linux.rs ~1931:

    for (r, vma) in self.vmas.overlapping(range.clone()) {
        mappings_to_change.push((r.start, r.end, *vma));   // whole VMA, NOT clamped
    }

It should push `(r.start.max(range.start), r.end.min(range.end))` — exactly the clamp already
applied to `remove_mapping` earlier the same day. Two instances of this idiom in one file
suggests it was copied; check every other `self.vmas.overlapping(...)` loop.

Why it fits the weston crash: the failing walk was
297193472 -> 297443328 -> 297455616 -> 297467904. The third fragment spans 12,288 bytes
(3 pages) but the page legitimately munmapped 617 ms earlier was only 4,096 bytes
(297455616..297459712). So the fragment runs 8,192 bytes PAST the freed page into the next
VMA — precisely what an unclamped whole-VMA extent produces. Windows then correctly refuses an
operation spanning a hole (ERROR_INVALID_HANDLE), and the runner's `assert!` turns that into a
process abort.

Crucially this needs NO stale bookkeeping and NO race: the VMA table can be perfectly accurate
and the walk still reaches memory the guest never asked about. Ruled out along the way, each
with hard evidence: the fresh-address TOCTOU theory (both paths do share
`ALLOCATE_PAGES_FIXED_ADDR_LOCK`, which aliases `VIRTUAL_PROTECT_LOCK`), the trampoline-mprotect
failure path (gone after the bitmask fix, crash unchanged), and oversized decommit (`len` is
clamped by `.min(range.len())`).

Real Linux semantics back the fix: `mprotect` affects exactly the requested range, splitting
VMAs at the boundaries. Applying to whole VMAs silently changes permissions on memory outside
the call even when nothing crashes. Test in the 5-second repro: if weston survives past
t≈5.97, that is the answer.

STRONGEST REMAINING LEAD (2026-09-03, traced but not yet tested): `deallocate_pages`
(lib.rs ~5574-5585) decommits with NO validation that the target is decommittable:

    |r, state| {
        debug_assert_ne!(state, MEM_FREE, "Trying to deallocate a free region");
        VirtualFree(r.start, r.len(), MEM_DECOMMIT)
    }

- The only check is a `debug_assert_ne!`, which compiles away entirely in release builds, so a
  release run decommits with no state check at all.
- Nothing checks `mbi.Type` for `MEM_MAPPED`. `VirtualFree(MEM_DECOMMIT)` is not valid on a
  section view, and it would leave exactly the observed end state: `MEM_MAPPED` +
  `MEM_DECOMMIT` + `protect=0`.
- Its call site (mm/linux.rs:909) runs only `if shared_overlaps.is_empty()`, i.e. when litebox
  believes no shared VMA overlaps. That fits the other evidence: the crashing range has no
  `diag-exec-mmap` entry, so it was not created through a path that registered it as shared. An
  unregistered view passes the guard and gets decommitted while live.

Hypothesis: some path creates a `MEM_MAPPED` view without a matching shared-handle VMA entry;
a later ordinary `munmap` in that range takes the "no shared overlap, just decommit" branch and
pulls the backing out from under weston. Test: log `mbi.Type` in that closure and skip/fail
loudly on `MEM_MAPPED`, then run the 5-second repro. The guard is correct regardless of the
root cause, and converts silent corruption into a loud attributable error. Also promote the
`debug_assert_ne!` to a real runtime check.

Historical detail on the (real, fixed, but non-causal) clamp bug, in the logical partial-unmap
added to `remove_mapping` (mm/linux.rs ~895):

    let overlap_start = range.start.max(view_range.start);
    let overlap_end   = range.end.min(view_range.end);
    let _ = self.platform.update_permissions(overlap_start..overlap_end,
                                             MemoryRegionPermissions::empty());

- Empty permissions map to `PAGE_NOACCESS` (prot_flags, lib.rs:4895), matching the observed
  `mbi_protect=0`.
- It is the one place today's changes act on a shared view without physically unmapping it, and
  it swallows errors (`let _ =`), so harm is silent.
- KNOWN SCOPING BUG, flagged and still unfixed: the clamp uses `view_range` (the WHOLE original
  view) instead of `vma_range` (the fragment actually being removed), so a partial `munmap` can
  revoke access across bytes belonging to a DIFFERENT, still-live fragment of the same view.
  That is exactly "a live process still has it mapped". Fix: clamp to `vma_range`.

Do not assume this is a pre-existing deep lifecycle bug: it surfaced in the same session as the
view_base/logical-unmap work, on a path that work introduced. Comparing one run with those
changes reverted is a 5-second experiment that separates regression from pre-existing.

Also fixed this pass (real, independent of the above): `sys_mprotect` in
`litebox_common_linux/src/mm.rs` matched only 5 exact flag combinations and returned EINVAL
(debug: `todo!()` panic) for the rest; now handles all 8 via the general permission path.
Confirmed NOT the cause of the weston crash — it still reproduces after the fix.

Reading `diag-vprotect` logs: `new_flags` there is the raw Win32 value passed to
`VirtualProtect`, NOT `MemoryRegionPermissions`. 1=PAGE_NOACCESS, 2=PAGE_READONLY,
4=PAGE_READWRITE, 32=PAGE_EXECUTE_READ. (A value of 32 is impossible for
`MemoryRegionPermissions`, whose bits stop at 15, which is the quick check.) Misreading these
as litebox permission bits produced two wrong conclusions before it was caught.

## 3D. END-OF-SESSION STATE (2026-09-03) — supersedes 3B/3C below

Read this section first; several conclusions in 3B/3C and earlier were overturned by later
evidence the same day and are kept only to show what was ruled out.

**Confirmed working.** weston's shell renders a COMPLETE, CORRECT desktop, reproducibly, in
multiple runs. Decoding `final_frame_dump5.28` directly: 2,012,160 pixels of exactly
(0,34,68) = the `background-color` from weston.ini, a 32px panel band (rows 0-31) at the
composited panel shade, and 465 light pixels spanning x=12..1904, i.e. panel content at both
ends of the full width. Compare the project's starting point ("an icon and time", ~1,100
pixels in rows 7-24). This is weston-desktop-shell, NOT XFCE; xfce4-session had not started
at that point in the run.

**The remaining display bug is a teardown, not a startup failure.** Content persists ~24-27
frames then blanks. At the transition both Wayland sockets keep exchanging messages normally
in both directions (writes ok, reads draining the same byte counts), there is no crash within
60 s, and the Xwayland fork happens AFTER the blackout. So it is a legitimate protocol event
on a healthy connection, most likely a surface unmap/destroy, a buffer released and not
replaced, or an output reconfigure. Next step: log the first 8 bytes (object id + opcode) of
each message alongside the length in `diag-unix-stream-write`, then read off which object
received which request at the blackout timestamp.

**Retracted the same day (do not re-derive):**
- "background=0 means the background surface is never created" — FALSE. It is a transient;
  rendering succeeds after that log line. Much earlier project notes rest on the wrong reading.
- "the Xwayland xkbcomp fork kills the content" — coincidence in one run; the later run has the
  fork after the blackout.
- "shared memory is corrupted by fork relocation" — `is_private_data_range` requires
  `shared_handle.is_none()`, so shared regions are never relocated.
- "the epoll observer registration is lost on the empty-events path" — observers persist in a
  map and are pruned only when dead.
- "unix.rs's connected-stream write does not notify the peer" — `channel.rs` `try_write_one`
  notifies `peer.pollee` on every successful push; verified wiring and a unit test exist.
- "litebox has a ~65% silent launch-failure rate" — a `/bin/sh -c` wrapper crashing; a direct
  tar-layer launch gives 50/50 clean runs.

**The open blocker with the best evidence: a deterministic guest `/bin/sh` SIGSEGV.** Two runs,
different address-space layouts, identical offset `0x1464b` into the faulting mapping, `rip ==
cr2`, `error_code=0x6` (user-mode, page-not-present), each within ~150 us of a fork plus a
`fixup_stale_elf_data_pointers` heal. Disassembling busybox (ET_DYN, so offset maps to file
offset) at `0x1464b` gives `leaq 0x148(%rbx), %rax`, the FALL-THROUGH of a 6-byte `jne` ending
exactly at `0x1464b`. `leaq` touches no memory, so this is an instruction-FETCH fault reached by
ordinary sequential execution — not pointer corruption. Framing for whoever continues: the
forked child's text page is not backed on the host while litebox's VMA table reports the region
mapped with MAYEXEC. Ask whether the child's text is committed across its full length, and how
fork populates it (fresh private copy, shared view, or CoW).

CRUCIAL identification (corrects an earlier reading in this file): the crashing `/bin/sh` is
NOT the launch script's shell. It is XWAYLAND's own forked child — `execve pid=17 ppid=14
argv0=/bin/sh` followed by `argv0=/usr/bin/xkbcomp`, where pid 14 is Xwayland — i.e. the
short-lived helper shell Xwayland spawns to compile its keymap. That is why the offset is
identical every time: same binary (busybox sh), same parent, same code path. It also means
runs are NOT truncated by it, contrary to what this file said earlier.

The practical payoff is a much smaller repro. The trigger shape is: a LARGE process (Xwayland,
296 relocation ranges, ~47,600 pointers healed) forks, and the child execs a small binary. That
should reproduce with Xwayland alone, no XFCE, in seconds instead of 80. A synthetic
large-address-space parent that forks and execs busybox would isolate it from Xwayland
entirely (build it with the host cross-compile recipe in `advisor/probes/README.md`).

Ruled out by a dedicated subagent (2026-09-03): `fixup_stale_elf_data_pointers` correctly
excludes `VM_EXEC` ranges; `Vmem::duplicate` has no partial-copy path (it copies whole regions
or hard-fails); both active `VirtualProtect` call sites hold `VIRTUAL_PROTECT_LOCK` across
their full span. A third, unlocked `VirtualProtect` exists in `fork_verify.rs`
`codewatch::protect()` (~line 2187) but is gated behind `LITEBOX_CODEWATCH`, unset in every run
here — a real robustness gap to fix, but not the active cause. The leading remaining hypothesis
is a protection change racing an instruction fetch; the tightest place to catch it is
instrumenting `VirtualProtect` target ranges plus thread ids across fork+exec of a large
process.

## 3C. Current blocker (2026-09-03, end of session): deterministic stop after liblzma

With the launch race fixed and both fixes in place, two runs stop at effectively the same
instant:

    pass_v3_ordering_fix.log:  last activity t=59.519, last library /usr/lib/liblzma.so.5
    pass_v3_longtimeout.log:   last activity t=59.638, last library /usr/lib/liblzma.so.5

A 0.12 s difference across runs with DIFFERENT timeout budgets, at the same library, with zero
futex/signal/exit activity after t=58. A longer budget produced no further progress, which
disproves the "it is just slow loading a 110 MB libLLVM" theory (my own reading of the steady
mmap rate as progress was wrong; progress is normal right up to a fixed point, then ceases).

This is the cleanest repro of the session: same stop, same library, same timestamp, twice, with
the whole XFCE library chain loaded and both xfwm4 and xfce4-session still alive and no
"cannot open display".

Attribution (corrected by timestamps, not load order): the stalling process is xfce4-session
(pid 50, execve at t=53.171), but the expensive Mesa chain is NOT its. `libGL` loads at t=10.07
and `libLLVM` at t=11.05, then again at t=46.96/48.14 — those are XWAYLAND's two startups, long
before xfce4-session exists. Xwayland legitimately uses libgbm/GL, and it completes: it goes on
to serve xfwm4 with no "cannot open display". What xfce4-session itself loads from t=53 is an
ordinary GTK/XFCE stack (libxfce4ui, libxfce4util, libxfconf, libxfce4windowing,
libgtk-layer-shell, libwnck, gtk-3, gdk-3, cairo, pango, atk, gio, glib, libICE, libSM,
libstartup-notification, libXrandr); the trailing libpciaccess/liblzma are transitive
(libdrm, libelf/libxml2) in an interleaved log stream, not a Mesa JIT load.

So the GL chain is demonstrably not fatal, and "disable GL" is unlikely to be the fix
(`GDK_GL=disable` and xfwm4 compositing off are still worth setting, but they will not remove
the libLLVM cost, which is Xwayland's and happens ~40 s earlier). The stall is most likely
inside xfce4-session's own GTK/XFCE initialisation right after that library set finishes —
exactly where the ORIGINAL futex deadlock sat (xfce4-session, after libxfce4ui, parked on a
lock owned by no live thread). The two look MORE alike, not less.

Highest-value next probe: `XFSM_VERBOSE=1`, which makes xfce4-session write its own startup
trace to `$HOME/.xfce4-session.verbose-log` naming the exact stage reached (see Appendix C).
Retrieve that file from the writable layer after the run. liblzma is pulled in by the Mesa/DRI chain, so the cheapest next step is
to avoid GL entirely (`GALLIUM_DRIVER=softpipe`, `GDK_GL=disable`, xfwm4 compositing off): if
the chain is never loaded and the stop disappears, the blocker is confined to the GL path
rather than XFCE. Otherwise capture the last syscall per thread after t=55 at debug level, or
take a fatal-dump/VEH trace of the final seconds to get the spinning or faulting rip.

## 3A. Reading run-to-run differences: a queue of blockers, not "non-determinism"

Repeatedly this session, differing outcomes across runs were labelled non-determinism and set
aside; each time, isolating one variable found something specific. The pattern to recognise is
that the launch sequence is a QUEUE of sequential blockers, so what varies between runs is how
far you get, not whether the code is deterministic.

Worked example (2026-09-03). Three pre-fix baseline runs all gave `exit=124`, all stopped at
the identical last line (Xwayland's xkbcomp warning), all with zero `diag-unrecov` and zero
`diag-fv-lifecycle`. That is not variability, it is a perfectly reproducible blocker, and three
identical failures is the most tractable state an investigation can be in. Meanwhile an earlier
run reaching 72 `diag-unrecov` was not a different random outcome; it simply got past this
point and hit the next blocker.

Follow-up that corrects the example above: the three "xkbcomp hangs" were NOT a hang at that
point at all. Reading the twenty lines BEFORE the stop (postfix_1.log) shows
`Segmentation fault` immediately after weston's desktop-shell catchup-loop diagnostics, then
`(xfwm4:16): Gtk-WARNING: cannot open display: :1`. The X server died; xfwm4 had nothing to
connect to; the rest of the timeout is the script waiting. The xkbcomp warnings are ordinary
Xwayland keymap noise that merely happened to be the last output. Generalise: when a run
"stops", read the preceding twenty lines, never just the final one.

Sharper still, and a live warning as of 2026-09-03: `Segmentation fault` appears ONLY in the
post-fix runs (prefix_baseline_1/2/3 = 0, 0, 0; postfix_1 = 1; postfix_2 = 2, one of them right
after "seatd started"). That LOOKS like a regression, but a line-by-line comparison says
otherwise and the first reading was wrong. At the identical line 63 the baseline continues into
Xwayland's xkbcomp warnings and then stalls silently; the post-fix build segfaults and then
reaches `(xfwm4:16): cannot open display :1`. xfwm4 appears NOWHERE in any baseline log. So the
post-fix build is the only one getting far enough to launch xfwm4, and is therefore reaching a
crash the baseline never reached, rather than newly breaking working code. A build that stalls
earlier trivially scores zero segfaults. Bisect anyway (baseline / view_base only / epoch only
/ both, three runs each), but interpret on DEPTH REACHED first and crash count second.

The generalisable rule, arrived at by getting it wrong twice in one investigation (first
accepting a truncated tail as the failure point, then treating a crash-count delta as
regression): never compare two runs on any metric without first establishing they reached
comparable depth. Here a single metric would have reported an improvement for a build that was
crashing more, and a regression for one that was progressing further.

Also verify a run actually captured litebox's own output before drawing conclusions from it:
these comparison runs contained ZERO `ERROR`/`diag-` lines, i.e. logging was effectively off,
so "0 diag-unrecov" meant "no instrumentation", not "no crashes". Run comparisons at
`LITEBOX_LOG=error` with `LITEBOX_DUMP_FRAMES` set, capturing per run: segfault count, which
process died, `diag-fv-lifecycle` counts including refusals, `diag-unrecov` count, and the last
meaningful line.

Consequences for method:
- A metric like the `diag-unrecov` count is meaningless on its own. Always report it together
  with how far the run got, since a run that dies early trivially scores zero.
- Never compare "3 hangs vs 3 hangs" to evaluate a fix whose code was never reached. Confirm
  once that the blocker is unchanged, then spend the time diagnosing the blocker.
- For any hang, the first question is SPINNING or BLOCKED, answerable from one debug-level run:
  repeating syscalls on a tid means a live loop (note that a healthy idle wait also loops, e.g.
  the 940k-line trace ends in a benign `EpollFile::wait`/`repoll_stdin_and_timerfd_interests`
  cycle every ~20 ms on tid=20, so distinguish idle-waiting-forever from a missed wakeup);
  total silence means a thread blocked in a syscall that never returns, and the last syscall
  per thread names it.
- Before blaming the emulator for a hang after process X starts, check the timeline for whether
  anything was launched after X at all. A script that starts a server and then nothing produces
  an indistinguishable "hang".

## 4. Process

- Parallel tracks, each owned by a subagent (gm with dynamic workflows), each with one
  falsifiable objective and one number: (1) strace + summary, (2) Xorg-on-KMS, (3) rootful
  Xwayland, (4) inotify + mremap + munmap semantics + the ENOSYS list from (1), (5) fork/
  FS-base stress repro and the 1.1 rewriter change, (6) headless frames + presenter process +
  control channel, (7) the 1.2 fork redesign staged as in 1.2.
- One canonical launcher (`dev_tools/gui_debug/run-xfce.ps1`) with all env, layers and
  timeouts fixed, writing a structured run directory (per-process logs, frames, strace
  summary, timeline). Every experiment is a one-liner, and subagents never share scratch tars.
- Reporting discipline: numbers before narrative. "Host non-determinism", "Heisenbug",
  "cumulative load" are not root causes; a bug that disappears under tracing is a race in the
  emulator, and tracing is a bisection lever, not a mitigation.

## Appendix A. libdrm calls made by xserver's modesetting driver (from source, 2026-09-03)

Counted over `hw/xfree86/drivers/modesetting/{driver,drmmode_display,vblank,present,dumb_bo}.c`
on xserver master. The non-atomic, non-glamor startup path (what track 2a exercises) needs the
ioctls behind: `drmGetVersion` (VERSION), `drmSetInterfaceVersion` (SET_VERSION),
`drmGetBusid` (GET_UNIQUE), `drmSetMaster`/`drmDropMaster`, `drmGetCap` (DUMB_BUFFER,
DUMB_PREFER_SHADOW, CURSOR_WIDTH/HEIGHT, TIMESTAMP_MONOTONIC, ADDFB2_MODIFIERS, PRIME),
`drmSetClientCap` (UNIVERSAL_PLANES; ATOMIC only with Option "Atomic"), `drmModeGetResources`,
`drmModeGetConnector`/`GetEncoder`/`GetCrtc`, `drmModeGetProperty`, `drmModeGetPropertyBlob`
(EDID and mode blobs), `drmModeObjectGetProperties` (connector, CRTC and plane properties;
modesetting reads plane "type"), `drmModeGetPlaneResources`/`GetPlane`,
`drmModeCreateDumbBuffer`/`MapDumbBuffer`/`DestroyDumbBuffer` (dumb_bo.c), legacy
`drmModeAddFB` (ADDFB, not ADDFB2, for the dumb front buffer), `drmModeGetFB`, `drmModeRmFB`,
`drmModeSetCrtc`, `drmModeDirtyFB` (driver disables dirty tracking on EINVAL/ENOSYS),
`drmModeSetCursor`/`SetCursor2`/`MoveCursor` (fail cleanly, or set Option "SWcursor"),
`drmModeCrtcSetGamma` (failure is logged and tolerated), `drmWaitVBlank`,
`drmCrtcGetSequence`/`QueueSequence` (optional, falls back), `drmModePageFlip` and
`drmHandleEvent` (Present extension only), `drmModeConnectorSetProperty`/`ObjectSetProperty`
(DPMS, tolerated). Atomic (`drmModeAtomic*`, `CreatePropertyBlob`), leases
(`drmModeCreateLease` etc.), `drmPrimeFDToHandle` and `drmModeAddFB2WithModifiers` are only
reached with atomic, leasing, or glamor/DRI3.

Because this driver draws into the mapped dumb buffer in place after one `SETCRTC`, the
scanout must be sampled continuously (section 1.3); `DIRTYFB` is a hint, not the trigger.

## Appendix B. Starting recipe for track 2a (Xorg on the virtual KMS device)

Packages (Alpine): `xorg-server` (modesetting is built in), `xf86-input-evdev` or
`xf86-input-libinput`, `xkeyboard-config`, `xkbcomp`, `font-misc-misc` and `font-cursor-misc`
(Xorg exits with "could not open default font 'fixed'" without them), `dbus`, `xfce4`
(`xfce4-session`, `xfwm4`, `xfce4-panel`, `xfdesktop`, `xfce4-settings`, `thunar`),
`elementary-xfce-icon-theme` or `adwaita-icon-theme`, `ttf-dejavu`, `xterm`.

`/etc/X11/xorg-litebox.conf`:

```
Section "ServerFlags"
    Option "AutoAddDevices" "off"
    Option "AutoAddGPU"     "off"
    Option "DontVTSwitch"   "true"
EndSection
Section "Device"
    Identifier "kms"
    Driver     "modesetting"
    Option     "kmsdev"      "/dev/dri/card0"
    Option     "ShadowFB"    "true"
    Option     "AccelMethod" "none"
    Option     "SWcursor"    "true"
    Option     "Atomic"      "false"
EndSection
Section "Monitor"
    Identifier "mon"
EndSection
Section "Screen"
    Identifier "scr"
    Device     "kms"
    Monitor    "mon"
    DefaultDepth 24
    SubSection "Display"
        Depth 24
        Modes "1920x1080"
    EndSubSection
EndSection
Section "InputDevice"
    Identifier "evdev0"
    Driver     "evdev"
    Option     "Device" "/dev/input/event0"
EndSection
Section "ServerLayout"
    Identifier  "layout"
    Screen      "scr"
    InputDevice "evdev0" "CoreKeyboard"
EndSection
```

Launch (as root inside the guest, after dbus):

```
Xorg :1 -config /etc/X11/xorg-litebox.conf -novtswitch -sharevts -noreset \
     -nolisten tcp -logfile /tmp/xorg.log &
export DISPLAY=:1
dbus-run-session -- startxfce4      # or: xfce4-session
```

Expected first blockers, in order: VT ioctls on `/dev/tty0` (already emulated for seatd),
the DRM ioctls in Appendix A (`ADDFB` legacy first), the evdev ioctl set for the single
combined keyboard+mouse node (one evdev device can serve both; if the server insists on a
separate pointer, add a second `InputDevice` with the libinput driver), then fonts. `xwd
-root -display :1 | convert` or `xwd` plus a tiny XWD-to-PNG step gives the guest-side ground
truth for 3.6.

## Appendix C. XFCE startup facts worth knowing (from xfce4-session source, master)

- `XFSM_VERBOSE=1` makes xfce4-session log every startup step to
  `$HOME/.xfce4-session.verbose-log` (xfsm-global.c). Use it before any tracing.
- Order after the D-Bus name is acquired: manager, environment, xfconf channel, XSMP/ICE
  listener (`/tmp/.ICE-unix/<pid>`, plus TCP unless `--disable-tcp`), gpg-agent/ssh-agent via
  `g_spawn_command_line_sync` (both daemonize; disable with xfconf
  `/startup/{ssh,gpg}-agent/enabled=false` while fork is unsound), session or failsafe load
  (needs `/general/FailsafeSessionName` via xfconfd and `XDG_CONFIG_DIRS` including
  `/etc/xdg`), then clients spawned per priority group with XSMP registration waits.
- Nothing prints before `bus_acquired`; an empty verbose log means the stall is in the X
  connection, `xfconf_init`, or `g_bus_own_name`.
- xfwm4 prints nothing on success. Its compositor (on by default) paints the whole screen
  through XRender, so a uniform frame after xfwm4 starts is expected, not a bug.

## Appendix D. Presenter process and control channel: implementable spec

Goal: the guest and the litebox kernel never depend on a window. Frames exist headless; a
presenter can attach, show, hide and detach at any time; tools can screenshot and inject
input without a window. Also removes winit/wgpu/COM state from the guest process (needed
for the clone-based fork) and the pass-308/309 presenter-vs-DRM race.

D1. Scanout publication. DRM dumb buffers are already platform shared-memory objects
(sections). The DRM device keeps a "scanout descriptor": {section handle, width, height,
pitch, format, buffer offset, frame_seq}. On `SETCRTC`/`PAGE_FLIP` it updates the
descriptor (new handle or offset) and bumps `frame_seq`; on `DIRTYFB` it bumps `frame_seq`
only. No pixel copy is needed on the kernel side. Consumers sample the live buffer at their
own rate (vkms model: scanout happens every vblank regardless of flips); tearing is
acceptable for a first version, and a double-buffered copy can be added later behind the
same descriptor.

D2. Control channel. Named pipe `\\.\pipe\litebox-<runner pid>` (Unix socket
`$XDG_RUNTIME_DIR/litebox-<pid>.sock` on Linux/macOS), one text command per line, one text
reply per line, optional binary payload with a length prefix:
- `scanout` -> `ok <duplicated handle or name> <w> <h> <pitch> <format> <offset> <seq>`;
  on Windows duplicate the section handle into the caller's process (the pipe gives the
  client pid) so the caller maps it directly.
- `screenshot <path>` -> the kernel writes a PNG (stored-deflate PNG encoder is ~80 lines,
  no dependency) or BMP of the current scanout; `ok <path> <non_black> <distinct>`.
- `show` / `hide` -> spawn the presenter if absent, or hide/show its window; `presenter?`
  reports state. `--gui` at launch means `show` at startup; `--gui=hidden` starts a presenter
  that stays hidden (window exists, not visible); no flag means headless until asked.
- `key <evdev code> <0|1>`, `rel <dx> <dy>`, `wheel <n>`, `abs <x> <y>` -> injected into the
  evdev queue exactly as presenter input is today.
- `ps`, `strace on|off [pid]`, `kill <pid> <sig>`, `stats` -> diagnostics from section 3.
- `frames on|off <dir>` -> kernel-side frame dump toggle (LITEBOX_DUMP_FRAMES at startup).

D3. Presenter executable (`litebox-presenter`, same repo, own `main`): connects to the
pipe, requests `scanout`, maps the section, creates the winit window on its own main thread
(Cocoa-safe on macOS), uploads the buffer to wgpu at the window's refresh rate or when
`frame_seq` changes (poll a `frame_seq` mirror in a small shared header page, or wait on a
named event the kernel signals on flip), forwards keyboard/mouse over the pipe as `key`/
`rel` lines, handles `hide`/`show` from the pipe and a hotkey (Ctrl+Alt+H). Crashes in the
presenter never affect the guest; the kernel just drops the connection.

D4. Kernel side changes: a `ControlServer` thread owning the pipe listener, a
`ScanoutDescriptor` in `DrmSubsystem` updated from the existing flip/setcrtc paths, the
evdev injection entry points already exposed by the shim (`lib.rs` ~381-391), and the
frame-dump writer moved from presentation.rs into the shim/runner. The current in-process
presenter can stay as a fallback behind a flag until the new one is verified, then be
deleted along with its thread-stack special-casing.

D5. Verification: (1) headless run, `screenshot` via the pipe every 5 s from a host script,
frame_stats on the result; (2) start hidden, `show`, human sees the window, `hide`, guest
keeps rendering (screenshots keep changing); (3) kill the presenter with Task Manager, guest
unaffected, `show` brings a new one; (4) inject `key` lines to type into xterm on :1 and
confirm via screenshot.

## Appendix E. Reading a stuck futex under musl (Alpine)

When a guest thread parks forever in `FUTEX_WAIT`, the expected value tells you which lock
and what went wrong (musl 1.2.x source, verified 2026-09-03):

- `pthread_mutex` (owner-tracking types: errorcheck, recursive, robust): the word is
  `owner_tid | 0x80000000 (waiters) | 0x40000000 (owner dead)`; a waiter waits with
  `t = word | 0x80000000`. A wait value of exactly `0x80000000` means "waiters set, owner
  tid 0": the thread that locked it had `self->tid == 0` (`pthread_mutex_trylock` stores
  `self->tid`). Check tid plumbing: `set_tid_address` return, `CLONE_PARENT_SETTID`, and
  `*(int*)((char*)pthread_self()+0x30)` (musl x86_64 `struct pthread.tid`).
- `pthread_mutex` NORMAL type: values 0 / 16 (EBUSY) / `0x80000010` while contended.
- musl internal `__lock` (malloc ctx, ldso, atexit, thread list): held = `INT_MIN+1`
  (`0x80000001`), each waiter adds 1, waiters wait on the incremented value. A wait on
  exactly `INT_MIN` (`0x80000000`) is only reachable if the word passed through
  `0x7FFFFFFF`, which `__unlock` produces when the word was zeroed underneath it between its
  sign check and its atomic add. Something else wrote the lock word: a relocated-copy fork
  child resetting "its" locks through stale pointers in libc's `.data` is one such writer.
- GLib `GMutex` uses raw futex words 0 / 1 / 2 (contended waits on 2); `GCond` waits on its
  own sampler value; neither produces `0x80000000`.
- musl `dlopen` releases its rwlock before running constructors, so a constructor calling
  `dlopen` does not self-deadlock; a missing or failing library returns NULL, it never blocks.

Symbolize the futex address against the loaded-module list; for stripped Alpine libraries use
the `-dbg` package (`musl-dbg`, `glib-dbg`) or match the offset in `objdump -d` output.

Worked example (xfce4-session hang, 2026-09-03). Trace facts: `main_base=0x7d4b0000`, image
235,584 bytes; stuck wait at `addr=0x7d594980 val=0x80000000 timeout=None`; that address is
past the executable's own mapping and falls in the anonymous `.bss` tail of the library mapped
at `0x7d592000`, which the preceding `openat` identifies as `libxfce4ui-2.so.0`; the whole
940k-line trace contains exactly one line mentioning the address (the wait itself), so no
`FUTEX_WAKE` ever targeted it. libxfce4ui's `xsmp_ice_init` (xfce-sm-client.c) sits behind a
`g_once_init_enter` guard and calls `IceSetIOErrorHandler`/`IceAddConnectionWatch`; it runs
only on the X11 session-management path, which is why a no-display run of the same stack does
not hang. The guard itself is not the stuck word: GLib's `GMutex` is a raw futex with states
0/1/2 and `GCond` waits on a sampled counter, so neither can yield `0x80000000`. That value is
musl's `pthread_mutex` encoding with owner tid 0. libICE has no mutexes of its own
(globals.c/misc.c/watch.c), and GLib's `GMutex`/`GCond` are raw futexes, so the owner-tracking
lock is a `GRecMutex` (GLib allocates it as a `PTHREAD_MUTEX_RECURSIVE` pthread mutex whose
pointer lives in the owning library's data) or an Xlib lock of the same shape.

Where a tid-0 owner legitimately arises: musl sets `self->tid = 0` during THREAD EXIT
(pthread_create.c:117) and only afterwards walks the robust list to release the mutexes the
dying thread held (`a_swap(&m->_m_lock, 0x40000000)` plus a wake when waiters are set), then
runs `__do_orphaned_stdio_locks` and `__dl_thread_cleanup`. So a thread that dies without
completing that userspace cleanup leaves its mutexes owned by tid 0 forever, and any later
waiter parks on exactly `0x80000000` with no possible waker.

litebox's own tid plumbing checks out and is not the suspect: clone writes the real
`child_tid` through `CLONE_PARENT_SETTID` before spawning (process.rs ~2371) and the child
Task carries the same value (~3055); musl uses `CLONE_PARENT_SETTID` with `&new->tid` and
never `CHILD_SETTID` (pthread_create.c:245,355); `set_robust_list` and the robust-list walk
(process.rs ~1049-1135) match Linux, including the `FUTEX_TID_MASK` ownership check,
`FUTEX_OWNER_DIED` and the waiter wake.

Ruled out for this specific trace (2026-09-03): neither xfce4-session thread (tid 28 main,
tid 29 worker) ever exits before the hang, so "a thread died mid-cleanup" is not the local
explanation, though the rule below still holds generally.

The stronger candidate, and why it is a fork problem. musl's `fork()` has the CHILD write into
memory that on real Linux is its own private copy (src/process/fork.c):

    pthread_t self=__pthread_self(), next=self->next;
    pid_t ret = _Fork();
    if (need_locks) {
        if (!ret) {                              /* child only */
            for (pthread_t td=next; td!=self; td=td->next)
                td->tid = -1;                    /* every OTHER thread's struct pthread */
            ...
        }
        ...
        for (...) if (*atfork_locks[i])
            if (ret) UNLOCK(*atfork_locks[i]);
            else **atfork_locks[i] = 0;          /* child zeroes lock words outright */
    }

Under a same-address-space fork emulation, any of those writes that lands in the parent
corrupts live parent state: a mangled `tid` field yields a lock word owned by no live thread,
and a zeroed lock word is the out-of-protocol write that drives musl's internal `__lock`
through `0x7FFFFFFF`. Both produce a waiter that can never be woken. `advisor/probes/
forklock_probe.c` tests exactly this (child writes to an anonymous page, a `.data` word and a
stack word must all be invisible to the parent, across 16 successive forks).

Structural gap to check alongside it: `fork_verify` state is per-thread (`begin`/`end` operate
on the calling thread's TLS, lib.rs ~6935-6948) and `end_fork_child_verification` is called
from the exit and execve paths (process.rs ~1843, ~1855, ~4238). In the failing trace
xfce4-session is itself a process-fork child and `fork_verify` was still translating stale CODE
pointers 90 us before its `execve`. Verify by logging, not by reading, that (a) verification is
cleared on the execing thread and no other thread in that process retains a relocation map
describing the pre-execve address space, and (b) a thread created after `execve`
(`pthread_create` of tid 29 here) inherits no relocation state.

The crash that triggered the skip is itself a relocated-fork failure, not incidental noise.
The same log line reports pid 14 `dbus-launch` faulting at cr2=0x2be3b000, inside a mapping
flagged `VM_OWN_FORK_PADDING` spanning 0x2be3b000..0x2ce3b000. Per that flag's own doc comment
(litebox/src/mm/linux.rs:78-96) it marks the placeholder span `Vmem::duplicate` reserves for a
coherent-relocation group in a fork CHILD; bytes still carrying the flag are the inter-region
alignment gaps between relocated regions. So the guest dereferenced a pointer that resolved
into relocation padding: the stale-pointer-after-relocation signature, unhealed. The whole
causal chain is one bug: relocation yields a stale pointer -> it lands in padding -> the guest
faults fatally -> the fatal-signal path calls `end()` while a healer holds the borrow -> the
clear is skipped -> a 92-range map leaks. The epoch fix makes the leak harmless but does NOT
stop the crash.

Two qualifications on that run, from its tail. First, the crash did not halt the sequence:
`dbus-daemon` (pid 13) execve'd successfully at t=15.8079, a tenth of a second BEFORE
`dbus-launch` (pid 14) faulted, and library loads continue past t=16.1. `dbus-launch` is only
the wrapper that starts the daemon, so losing it may be survivable. Second, the run ended
host-side, not from the guest crash: 72 `diag-unrecov` lines, the first on ThreadId(19) with
`rip == fault addr == 0x7ff124a3e0c0` on a page whose `Protect=0x4` (PAGE_READWRITE, no
execute). That is an execute fault, i.e. control jumped into non-executable memory, on a
different thread. Plausibly linked to the leaked map:
`translate_stale_source_indirect_call_target` and its register-indirect sibling rewrite
call/jump destinations, and a map describing a dead address space would send control somewhere
arbitrary. Checked directly and the link is stronger than plausible: ThreadId(19)'s ENTIRE
history in that log is two lines, `begin ... range_count=25` at t=15.8334 and then the
`diag-unrecov-av`. It armed a 25-range map, never cleared it, and died with `rip == fault addr`
on a non-executable page. That is a thread crashing with a LIVE map, and it is a DIFFERENT
thread from ThreadId(18) whose clear was skipped, so two threads in one run carried maps into
trouble. The mechanism therefore does not require the skipped clear at all; a still-live map is
enough, and the skipped clear only makes it permanent. Measure the fix by the `diag-unrecov`
count across three post-fix runs, and check whether a "stale map refused" line fires on the
thread that used to crash, which would be causal rather than correlational evidence. CAVEAT on
that measurement: the pre-fix figure of 72 is a SINGLE run. Every other log in the repo reports
zero, but that is an artifact, they predate the instrumentation entirely (the 940k-line xfsm
trace contains no `diag-unrecov` or `diag-fv-lifecycle` lines at all). Since these runs vary
(hangs at 124, panics, exit=127), take two or three PRE-fix runs on the current build with the
same script and instrumentation to establish a real baseline distribution before comparing.
Otherwise post-fix zeros could look conclusive while proving little. Do not read surviving `diag-unrecov` lines as either fix failing. Worth
classifying automatically: a guest fault landing in a `VM_OWN_FORK_PADDING` range should be
reported as "stale pointer into relocation padding" by the fault report (item 3.5).

CONFIRMED 2026-09-03 with direct log evidence (`%TEMP%\pass_forkverify_diag_run.log`, 474
lines, instrumented run). Counts: 13 `begin`, 26 `end (cleared)` (11 with `had_map=true`, 15
no-ops), 1 `end (SKIPPED clear -- already borrowed, map left live)`. The decisive detail:
ThreadId(18) has EXACTLY TWO lifecycle events in the entire run, a `begin` arming 92 ranges at
t=15.7465 and the skipped clear at t=15.9299, with nothing afterwards. That 92-range map leaked
permanently, not "for one exception cycle". The skip happened while the guest took a genuine
SIGSEGV (fault into a `VM_OWN_FORK_PADDING` guard range, pid 14 `dbus-launch`) and the task was
being torn down by the fatal-signal path, so the in-code justification ("the outer healer's
borrow is about to be dropped when it returns") does not apply: that thread never returns
through the healer. The `end()` path is also cheap and usually redundant (15 of 26 clears were
no-ops), so making it robust costs essentially nothing.

Sharpest lead as of 2026-09-03, found in litebox's own code: `fork_verify::end()`
(fork_verify.rs ~2521) clears the thread's relocation map through `try_borrow_mut`, and when the
`RefCell` is already borrowed it SKIPS the clear and logs "end (SKIPPED clear -- already
borrowed, map left live)". The in-code justification is that a nested fault during a
stale-pointer healer holds the outer borrow and will drop it on return, so the map is stale
"one exception cycle longer, never permanently". That holds for the healer-re-entry case it was
written for, but NOT for the `execve` call site (process.rs ~4238), which exists precisely
because "execve replaces the address space wholesale". A skipped clear there leaves a map
describing the pre-execve address space live in the new program, and nothing re-arms or
re-clears it, since `begin()` only runs on a fresh fork. Every later healing decision on that
thread then consults ranges describing unrelated memory, and a translated write through such a
map is exactly the out-of-protocol write that leaves a lock word owned by no live thread. The
timeline fits: healing was translating stale CODE pointers 90 us before xfce4-session's execve,
i.e. when the borrow is most likely held.

Fix regardless of whether it is the cause here: the execve path must not tolerate a skipped
clear. Either retry until the borrow is available, or stamp each map with a generation at
`begin()` and check it before any translation, so a map that outlived its address space can
never be applied and the condition becomes detectable instead of silent. Put the check inside
each of the four `translate_stale_*` healers (lib.rs ~1712-1802), which today consult the map
on any access violation with no validity check at all beyond a livelock counter.

Trap to avoid when implementing the stamp: it must NOT be a single process-wide counter bumped
in `end()`. Verification windows genuinely overlap across threads. From the same instrumented
log: ThreadId(6) and ThreadId(7) both hold live 14-range maps at t=1.578/1.588 and (7) ends at
t=1.634 while (6) is still verifying until t=1.743. A global bump on `end()` would invalidate
(6)'s valid map, turning a rare silent corruption into frequent silent failure-to-heal. `end()`
also fires on threads holding nothing (ThreadId(12) three times in a row at t=14.1406-14.1408,
and 15 of 26 clears were no-ops), so those would burn epochs for free. Compare each map against
the OWNING THREAD's address-space generation, bumped at `execve` and task termination only; or
store the address-space identity (task identity plus exec counter) in the map and compare that.

Still live after both fixes (2026-09-03, pass_v5_final.log): a guest `/bin/sh` SIGSEGV that is
a post-fork relocation fault, not a shell bug. The sequence, within 400 microseconds:

    5.622264  fixup_stale_elf_data_pointers summary ranges_seen=5 healed_count=238
    5.622595  diag-fv-lifecycle: begin tid=ThreadId(16) range_count=14
    5.622644  diag-guest-exception: Exception(14) rip=0x10ba464b kernel_mode=false
    5.622658  mapping overlapping cr2 range_start=0x10b90000 range_end=0x10bc7000 VM_READ|VM_M...
    5.622671  fatal signal Signal(11) pid=12 comm=sh

The child faults 49 us after its relocation map is armed, on a page fault whose address lands
inside a MAPPED readable region (not unmapped memory), directly after 238 pointers were
rewritten. Same family as the `dbus-launch` fault into `VM_OWN_FORK_PADDING`. The signature to
watch is a high `healed_count` immediately followed by a fault: it suggests the fixup rewrites a
pointer to a plausible-but-wrong address rather than leaving it stale.

Operational significance: every launch script is a shell script, so a `/bin/sh` that can die
mid-script silently truncates the run, which then reads as "stalled after weston" rather than
"script died". Several apparent hangs today have that shape. It also means the fork machinery
still produces real faults with both fixes applied, so those fixes should not be described as
resolving the corruption class.

Rule this implies: **a guest thread must never disappear without running the same cleanup
Linux performs on thread death** (robust-list release with waiter wakes, `clear_child_tid`
write plus wake). Any path that tears a thread down otherwise (a swallowed guest fault, a
teardown kill, a fork child abandoned mid-flight) silently orphans every lock it held and
hangs the next waiter. This is another argument for process-per-guest-process fork, where the
host kernel guarantees this processing.

## 5. Verified on this host, and things still to verify

Verified 2026-09-03 with `advisor/probes/clone_probe.c` (Windows 11 10.0.26200): `RtlCloneUserProcess`
succeeds; private memory is copy-on-write isolated; a `SEC_RESERVE` section view mapped before
the clone stays shared, and a page the parent commits after the clone is visible to the child
(this is exactly the memfd growth mechanism section 1.4 needs); inherited pipe handles work
from the child; both `NtCreateThreadEx` and kernel32 `CreateThread` work in the child; the
child's exit code is delivered. Keyed events do not rendezvous across processes.

Also verified 2026-09-03 with `advisor/probes/dup_probe.c`: the runner can hand a scanout
section HANDLE to a separately spawned presenter process via `DuplicateHandle`
(`DUPLICATE_SAME_ACCESS`) with no admin rights, no `OpenProcess` and no `SeDebugPrivilege`,
because the creator already holds a full-access handle to the process it spawned; the child
maps it and reads the correct pixels, and the child's writes are immediately visible to the
parent. This clears risk 1 of `docs/presenter-process-design.md`, so the zero-copy scanout in
Appendix D is proven end to end and implementation can proceed.

RETRACTED (2026-09-03): an earlier revision of this section claimed litebox had a ~65% silent
launch-failure rate, based on 13 of 20 runs of `forklock_probe` producing no output. That was
wrong, and the error was in the test harness, not litebox. Those runs were launched through a
`/bin/sh -c` wrapper doing runtime base64-decode + chmod; `/bin/sh` itself was crashing with
SIGILL partway through the wrapper, and its restart noise looked like silent launcher failures.
Isolating the variable settles it: `hello_probe` copied into its own minimal tar layer and
invoked directly as the runner's top-level program (`-- /hello_probe`, no shell, no base64, no
wrapper) gives 50/50 runs with `exit=0` and output exactly `SMWX`. **litebox's guest-launch
path is reliable.**

Methodology rule this earns: never launch a probe through a runtime-constructed shell wrapper.
Bake the binary into a tar layer and invoke it directly. Two separate harness bugs
(MSYS2 argument path-mangling, needing `MSYS2_ARG_CONV_EXCL="*"`, and this shell-wrapper
crash) have each produced convincing but false claims about litebox itself. Any measurement
taken through the old wrapper pattern is suspect and should be re-run before it is trusted,
including the "deadlock repro hits the pass-205 VEH crash" observation.

`advisor/probes/hello_probe` (1,192 bytes, staged markers `S`/`M`/`W`/`X`) remains useful as
the minimal launch-reliability check: no output at all would mean the guest never reached its
first instruction.

Still to verify:
- `QueueUserAPC2` availability on this build for cross-process signal delivery.
- The exact ioctl list modesetting needs (2a): enumerate from source.
- Whether Alpine's Xorg build has glamor compiled in (if so, `Option "AccelMethod" "none"`).

## 3F. Deterministic #UD in a trampoline during fork_verify (supersedes 3E's "shell fragility")

Evidence from pass_weston5.log and pass_weston6.log, two independent runs:

    rip=0x7feffff7fb8a  rsp=0xcf9f6b8  cr2=0x0  error_code=0x0  Exception(6)  Signal(4) SIGILL  pid=7 comm=sh

Both registers are bit-identical across runs with different layouts. This is deterministic,
not intermittent, and my earlier "four different signals, broad shell fragility" framing is
withdrawn for this failure.

**Exception(6) is #UD, not a page fault.** cr2=0 and error_code=0 confirm it: a page fault
always sets cr2 and a nonzero error_code. So rip is MAPPED and READABLE, and the bytes there
simply do not decode. The earlier reading ("jumped to unmapped memory, instruction-fetch
fault") is withdrawn. litebox's "NO mapping overlaps cr2" line refers to cr2=0 and is
meaningless here.

**rip is in the trampoline band.** `mm/linux.rs:2180` allocates top-down from
`TASK_ADDR_MAX - length`; `mm.rs:1906` notes this sits just below the host allocator region;
`mm.rs:1406` sets `state.trampoline_addr` from that allocation. The faulting rip is 449 KB
below TASK_ADDR_MAX, inside that band. The guest is executing a syscall trampoline stub whose
bytes are wrong.

**The task never reached execve.** No `DIAG_TIMELINE execve pid=7` line exists in the run.
pid=6 execs dbus-uuidgen and exits; pid=8 execs /bin/sleep. pid=7 exists only to die, comm
still "sh". It is the forked child of `dbus-daemon &`, dying between fork and execve. That is
why dbus never starts and every downstream readiness poll times out.

**It dies inside a live fork_verify pass.** The line immediately after the fatal signal is
`diag-fv-lifecycle: end (cleared) tid=ThreadId(11) had_map=true range_count=18` — the
relocation map was still active and was torn down only because the task died. The preceding
90 ms contain nothing but `write_usize_fault_tolerant` widen/restore pairs.

**Hypothesis:** fork_verify's pointer healing writes into, or fails to fix up, trampoline
stub bytes in the forked child, which then executes a half-rewritten stub. Fixed rip AND
fixed rsp fit this exactly (same stub, same call depth) and are inconsistent with a wild jump.
Trampolines hold generated code, not guest data pointers, so healing must never touch them.

Checks, cheapest first:
1. Dump 16 bytes at the faulting rip and diff against what `maybe_patch_exec_segment` wrote
   into that stub in the parent. A difference proves the corruption outright.
2. Log each process's `trampoline_addr`+len; assert no fork_verify write address falls inside
   any trampoline range. Exclude trampoline pages from healing if one does.
3. Check whether the child inherits the parent's trampoline mapping and range list
   (`range_count=18` in both runs suggests a fixed inherited set).

This is Windows-specific by construction: fork_verify exists only because Windows fork
emulation shares one real address space. It blocks the FIRST backgrounded service in every
launch script on either compositor, so it gates dbus -> seatd -> weston -> Xwayland -> XFCE.

**Retracted here:** bgshell_probe.sh was written for an intermittent problem that turns out to
be deterministic; it is no longer the right next test. The fast repro is instead: fork from a
shell whose binary went through syscall patching, do a little work in the child before execve.

## 3G. THE XFCE BLOCKER: concurrently-live forked children (supersedes 3F's attribution)

Four-way controlled experiment, identical work, 4 runs per condition, on a BARE alpine rootfs
with no weston/X/XFCE at all, sub-second per run:

    CONDITION                       FAULTS/run    max healed_count
    sequential, no "&"              0,0,0,0       plateaus 256
    backgrounded + wait (10 procs)  0,0,0,0       PINNED at 231 every run
    backgrounded, no wait (10)      2,1,0,1       climbs 222 -> 293
    backgrounded, no wait (30)      5,3,3         499

**"&" is innocent.** `/bin/true & wait` in a loop is 4/4 clean and its healed_count is pinned at
exactly 231 on every run. Remove only the `wait` and it crashes. Forking, backgrounding, job
control and execve all work. The trigger is CHILDREN BEING ALIVE AT THE SAME TIME.

**Clean dose-response.** 10 -> 30 concurrent children takes faults from ~1 to ~3.7 per run and
healed_count from 293 to 499, monotonically. That is state accumulation, not timing luck, and it
refutes "host non-determinism" for this failure class.

**The tell.** `fixup_stale_elf_data_pointers` healed_count CONVERGES and stops growing when
children are serialised, but grows without bound when they overlap. Suspect the relocation/heal
range set is per-address-space rather than per-child, or that a child's ranges are not retired on
exit while siblings are still live. On Windows every child shares ONE real address space, which
is exactly where that assumption breaks.

**Regression oracle**, 5 lines, sub-second:
`i=1; while [ $i -le 30 ]; do /bin/true & i=$((i+1)); done; sleep 2`
Pass = zero "fatal signal" lines over 3 runs (currently 3,3,5). Paired control: add `wait` after
`&`, which must stay at 0 with healed_count pinned.

**Why this is the blocker.** Every launch script backgrounds dbus, seatd, the compositor,
Xwayland and several XFCE components, all alive at once, at higher concurrency than this test.
It explains dbus never coming up in the weston runs while the same commands work individually,
and it explains the apparent randomness: fault count scales with how many services are live.

### Corrections to 3F (both retracted, by measurement)
- "fork_duplicate downgrades child text to PAGE_READONLY": FALSE. Of 238 duplicated ranges,
  exec ranges skipping the permission restore = 0. Source flag word == crash flag word.
- "text VMA is missing VM_EXEC": FALSE. The 0x71 flag word is a CORRECT read-only rodata
  mapping. The mmap trace shows the faulting 0x36000 range begins exactly where musl text ends
  (0x3e2d000) at the matching file offset (0x6d000), mapped PROT_READ as it should be.
- Also ruled out by measurement: `mm/mod.rs:1458 register_existing_mapping` (0 calls in a
  crashing run) and `apply_trap_fallback` (correctly restores RX).

### Two real usability bugs found while doing this
- The runner's program path must be RELATIVE (`bin/sh`, not `/bin/sh`). A leading slash gives
  ENOENT which then STACK-OVERFLOWS the runner at `lib.rs:679` (`load_program(...).unwrap()`),
  hiding the real error entirely. Normalise the path and return the error instead of unwrapping.
- Exit code hides the bug: runs with exit=0 still killed children. Any harness checking only the
  exit code will report false success. Count "fatal signal" lines instead.

## 3H. MECHANISM: concurrent fork_verify healing passes (41-run statistical result)

Pair `diag-fv-lifecycle: begin`/`end` in any run log to count how many fork_verify healing
passes are LIVE SIMULTANEOUSLY. Across 41 runs spanning all conditions:

    MAXCONCURRENT == 1 :  8 runs, ALL ZERO faults          (8/8 clean)
    MAXCONCURRENT >= 2 : 33 runs, 27 crashed (82%), up to 13 faults

**Zero counterexamples: no run with a single healing pass has ever crashed.** The 6 clean runs
at >=2 fit a race needing the right interleaving, not every overlap. Concurrent healing passes
are a NECESSARY condition for the crash.

Per-condition:

    CONDITION                       MAXCONCURRENT   FAULTS
    A  sequential, no "&"                 1            0
    B  "&" + wait                         1            0
    C  "&" spaced, short-lived            2            1
    E  "&" spaced, long-lived             2            0
    G  "&" fast x30, /bin/true            6            5
    D  "&" fast x10, /bin/true            7            2
    F  "&" fast x30, sleep 8             13            5

**Why mechanistically right:** fork_verify heals stale pointers by SINGLE-STEPPING the child,
and on Windows every guest process shares ONE real address space. Two single-step passes running
at once, each setting the trap flag and rewriting pointers in that shared space, is exactly the
shape that corrupts a peer. Adjacent hazards are already documented in the tree:
`process.rs:2710` (proactive fixups deliberately NOT applied cross-process because they would
write into the wrong address space) and `process.rs:1287`
(`residual-second-fork-verify-corruption-bug`). Same family, triggered by concurrency rather
than by a second sequential fork. It also explains the victim profile: always a forked child
that has not yet reached execve, i.e. exactly fork_verify's active window.

**Recommended test:** serialise fork_verify with one global lock held begin-to-end, so at most
one pass runs at a time. If MAXCONCURRENT drops to 1 and faults go to 0 on condition F, the
mechanism is confirmed; the real fix is then per-child healing state or scoping the single-step
so passes cannot interfere. NOTE: whether holding a lock across a single-step walk can deadlock
against the traced thread needs care -- that is why this was not attempted from the advisor side.

### Disproven by implementing them (do not retry)
- **Serialising execve does NOT help.** A global spinlock around the entire execve
  address-space transition (taken before `kill_other_threads()`, held through `load_program`)
  gave 3,3,4 faults vs 5,3,3 without. The damage happens BEFORE execve.
- **`LITEBOX_VEH_TRACE=1` does NOT suppress this crash and makes it WORSE**: 1,7,13 faults vs
  5,3,3 without. This contradicts the earlier session belief that VEH_TRACE mitigates crashes;
  that belief does not generalise to this failure.
