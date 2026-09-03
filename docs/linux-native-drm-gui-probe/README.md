# Full DRM->wgpu pipeline: real Linux-native run-verification

Closes the strongest remaining gap in the Linux GUI-support track: the
Linux-native equivalent of this session's opening milestone (a real guest
program driving litebox's DRM emulation with a real host window showing the
exact pixels, screenshotted and confirmed on Windows) had never been
attempted for `litebox_runner_linux_userland` (the runner that runs
*natively* on Linux, as opposed to `litebox_runner_linux_on_windows_userland`
hosting Linux guests from a Windows process). The presentation layer alone
was run-verified earlier (`docs/linux-presenter-run-probe/`, a synthetic test
pattern, no real litebox guest process) -- this closes the full pipeline.

## Result: pixel-perfect, twice, independently confirmed

Two separate runs, two distinct solid colors, both confirmed via a real
screenshot (`import` against the actual X11 window, not inferred from
"no crash"):

- `screenshot-solid-green.png`: `srgb(0,255,0)` at three sampled points
  (top-left, center, bottom-right)
- `screenshot-solid-blue.png`: `srgb(0,0,255)` at the same three points

Both exactly match the raw bytes the guest program wrote into the DRM dumb
buffer (`XRGB8888` byte order: byte 0 = blue, byte 1 = green, byte 2 = red).

## What was built and run

`drmgui.c`: a raw-ioctl DRM guest test program (no libdrm dependency,
struct layouts copied verbatim from litebox's own `litebox_common_linux`
`DrmMode*` types and the real kernel `drm.h`/`drm_mode.h`). Exercises the
full pipeline: `open("/dev/dri/card0")` -> `GETRESOURCES` (two-call
size-probe pattern) -> `GETCONNECTOR` (same pattern, reads the real
1920x1080 mode litebox's virtual display advertises) -> `CREATE_DUMB` ->
`MAP_DUMB` + real `mmap()` -> write a solid color into every pixel ->
`ADDFB2` -> `SETCRTC` -> `PAGE_FLIP` -> sleep 8s so a `--gui` host window has
time to actually render before the process exits. Color is overridable via
`DRMGUI_COLOR=B,G,R` (needs `--forward-env` on the runner CLI to actually
reach the guest, since environment is NOT auto-inherited by default).

Built natively (no cross-compilation needed for the guest program -- WSL2 has
a real `musl-gcc`): `musl-gcc -static -O0 -o drmgui drmgui.c`, then rewritten
with `litebox_syscall_rewriter` (built natively on the Windows host via plain
`cargo build --release`, since ELF rewriting itself is OS-agnostic byte
manipulation), then appended into a copy of `alpine-rootfs.tar` at
`tmp/drmgui.hooked`.

`litebox_runner_linux_userland` itself was built via `cargo zigbuild --target
x86_64-unknown-linux-gnu` (this session's established zig-based cross-linker,
already proven for the presenter probe) and copied into WSL2 alongside the
guest tar.

Run: `./litebox_runner_linux_userland -Z --forward-env --initial-files
rootfs-gui.tar --program-from-tar --gui -- /tmp/drmgui.hooked` against WSLg's
real X11 display (`DISPLAY=:0`). **Program path needs a LEADING slash** here
(`/tmp/drmgui.hooked`), the OPPOSITE convention from
`litebox_runner_linux_on_windows_userland`'s own established no-leading-slash
gotcha -- confirmed live via the CLI's own error message
(`--program-from-tar requires an absolute path`).

## A real, previously-undiscovered litebox bug found and fixed

First run failed: `CREATE_DUMB FAILED rc=-1 errno=12 (Out of memory)`,
preceded by `WARNING: disallowed syscall invoked: 319` (syscall 319 =
`memfd_create` on x86_64) on stderr. This is `litebox_platform_linux_userland`'s
own SIGSYS handler for its host-process seccomp filter (a security sandbox
restricting which real syscalls the host runtime itself may issue) --
`memfd_create`'s SIGSYS trap corrupts the register state the platform's own
`create_shared_memory` call was mid-syscall on, so the subsequent `CREATE_DUMB`
ioctl (which allocates its backing storage via that same function) fails.

**Root cause**: `create_shared_memory` (`litebox_platform_linux_userland/src/lib.rs`
~2610-2620) issues real `Sysno::memfd_create` and `Sysno::ftruncate` syscalls
directly to back litebox's own shared-memory objects (the same primitive DRM
dumb buffers and `memfd_create`-backed guest mappings both use) -- but neither
syscall number was in the platform's seccomp allow-list. This gap was
invisible until now because nothing on native Linux had exercised
`create_shared_memory`'s real-memfd path in a way that actually ran (as
opposed to `cargo check`/`cargo build`) until this exact `--gui` DRM test.

**Fix**: added `libc::SYS_memfd_create`/`libc::SYS_ftruncate` to the x86_64
seccomp allow-list (safe to allow unconditionally, same reasoning as the
existing `close`/`dup`/`fstat` entries right above them: on x86_64 every GUEST
syscall is intercepted by the ELF-patched fast-path trampoline before it ever
reaches the kernel/seccomp, so a trapped call to either of these two syscall
numbers is unambiguously host-originated) and to the aarch64
`aarch64_proxy_host_syscall_if_applicable` `PROXIED` array (same rationale,
aarch64's equivalent host-vs-guest disambiguation mechanism since it has no
fast-path trampoline yet).

**Verified**: full DRM pipeline succeeded after the fix (`CREATE_DUMB
handle=1 pitch=7680 size=8294400` -- exactly `1920*4` and `7680*1080`, the
real full-resolution buffer), and the resulting window's pixels matched the
guest's writes exactly (see screenshots above). Both `x86_64-unknown-linux-gnu`
(the actual host used for this test) and `aarch64-unknown-linux-gnu`
(type-check only, no aarch64 hardware in this environment) build clean;
clippy clean (`-Dwarnings`) on the touched crate.

## A non-bug, worth documenting to save future debugging time

An early attempt showed `panicked at ...lib.rs:3023:17: assertion left ==
right failed: signal 2 handler already installed`. This reproduced ONLY when
launching the runner via a backgrounded subshell (`(...&)`) inside a `wsl -e
bash -c` invocation that left a PREVIOUS run's process still alive in the
same WSL instance -- confirmed NOT a real bug via `pkill -f
litebox_runner_linux_userland` followed by a clean re-run, which succeeded
immediately. A background process started via `nohup ... & disown` inside one
`wsl -e bash -c` call also does NOT survive past that call's own exit (WSL
tears down the invocation's process tree) -- the working pattern is
launching, waiting, AND capturing the screenshot all within ONE `wsl -e bash
-c '...'` script (single-quoted, to avoid the outer shell's own `$`
interpolation stripping the inner script's variables).

## Reproducing this

```sh
# 1. Build the runner (from C:\dev\litebox-main, zig-on-PATH shim set up per
#    docs/wayland-drm-backend-probe/README.md)
cargo zigbuild -p litebox_runner_linux_userland --bin litebox_runner_linux_userland --target x86_64-unknown-linux-gnu

# 2. Build the syscall rewriter natively (plain host build, ELF rewriting is OS-agnostic)
cargo build --release -p litebox_syscall_rewriter --bin litebox_syscall_rewriter

# 3. In WSL2: compile the guest program with the real native musl-gcc
wsl -e bash -c "musl-gcc -static -O0 -o drmgui docs/linux-native-drm-gui-probe/drmgui.c"

# 4. Rewrite its syscalls (back on the Windows host)
./target/release/litebox_syscall_rewriter.exe drmgui -o drmgui.hooked

# 5. In WSL2: package into a copy of alpine-rootfs.tar, then run
wsl -e bash -c 'tar --owner=0 --group=0 -rf rootfs.tar drmgui.hooked --transform="s,^,tmp/,"'
wsl -e bash -c 'unset WAYLAND_DISPLAY; export DISPLAY=:0; export DRMGUI_COLOR=0,255,0
  ./litebox_runner_linux_userland -Z --forward-env --initial-files rootfs.tar --program-from-tar --gui -- /tmp/drmgui.hooked'
```

## evdev input injection, run-verified natively on Linux (`drmgui_input.c`)

Extends the same DRM pipeline with a real `select()`+`read()` loop on
`/dev/input/event0`, matching the exact live-witness discipline this session
already used to verify evdev input on Windows -- closing the platform-parity
gap between the two runners' `--gui` input support.

### A real, previously-undiscovered litebox bug found and fixed

The very first `WindowEvent::CursorMoved` winit delivers on this X11/WSLg
setup reports an implausible position (observed, reproducibly:
`(-32486, -32587)`, nowhere near any real screen coordinate) -- likely an
`EnterNotify`-adjacent quirk of WSLg's Weston window manager reporting a
position before the window is fully mapped. `presentation.rs`'s cursor-delta
code treated that bogus reading as a legitimate `last_cursor_pos` baseline,
producing a spurious `REL_X`/`REL_Y` delta of `32800` on the very next (real)
`CursorMoved` -- confirmed live via a temporary diagnostic printing the raw
`last`/`new` coordinate pair, then reverted. A guest program watching
`/dev/input/event0` for real input would see this fake event before any
input was actually sent, indistinguishable from genuine motion.

**Fixed** (`litebox_platform_linux_userland/src/presentation.rs`): a
`CursorMoved` position outside the window's own known client area
(`[0, surface_size.width) x [0, surface_size.height)`, already tracked in
`GpuState`) is discarded rather than accepted as a new `last_cursor_pos`
baseline -- a real cursor position is always within the window's own bounds,
so this correctly filters the pre-map garbage reading without introducing a
fragile magic-number threshold. Live-reproduced before the fix (identical
`32800` delta on every run, deterministic) and confirmed resolved after
(rebuilt, reran -- no more implausible deltas; legitimate window-manager-driven
motion, e.g. from `xdotool windowactivate`'s own cursor warp, still passes
through correctly since it's a real in-bounds coordinate).

### Live-verified: real X11-injected keyboard input reaches the guest

`drmgui_input.c` runs the identical DRM setup as `drmgui.c`, then opens
`/dev/input/event0` and waits for a real `EV_KEY` event (window-manager-driven
`EV_REL` cursor motion is deliberately NOT treated as the awaited signal --
only a deliberately-injected keypress counts, avoiding a false-positive early
exit before real injected input arrives). Following this session's own
established lesson about guest boot-latency races (see the wgpu/GUI-support
project memory's "always insert a real settle delay" note), input was
injected via `xdotool key --window <id> a` a few seconds after launch, once
the window was confirmed present and activated.

**Result**: `EVENT type=EV_KEY(1) code=30 value=1` -- `KEY_A` (Linux keycode
30), press (`value=1`), exactly matching what was sent, followed by its
`SYN_REPORT` and a clean exit. Real, unmodified-shape guest code correctly
decoding a real X11-injected keypress delivered through litebox's evdev
emulation, on native Linux -- the same rigor as the session's original
Windows `SendInput`-based evdev verification.

### Reproducing the evdev test

```sh
wsl -e bash -c "musl-gcc -static -O0 -o drmgui_input docs/linux-native-drm-gui-probe/drmgui_input.c"
./target/release/litebox_syscall_rewriter.exe drmgui_input -o drmgui_input.hooked
wsl -e bash -c 'tar --owner=0 --group=0 -rf rootfs.tar drmgui_input.hooked --transform="s,^,tmp/,"'

# Launch in the background of one wsl invocation, inject from a second concurrent one --
# backgrounding the runner INSIDE one wsl -e bash -c script (via `&`) reliably trips a real,
# unrelated environment quirk (a spurious "signal 2 handler already installed" panic, seen even
# on a fresh WSL instance) that a separate concurrent `wsl -e bash -c` invocation avoids.
wsl -e bash -c 'unset WAYLAND_DISPLAY; export DISPLAY=:0
  timeout 25 ./litebox_runner_linux_userland -Z --forward-env --initial-files rootfs.tar --program-from-tar --gui -- /tmp/drmgui_input.hooked' &
sleep 6
wsl -e bash -c 'unset WAYLAND_DISPLAY; export DISPLAY=:0
  WINID=$(xdotool search --name "litebox virtual display" | head -1)
  xdotool windowactivate --sync "$WINID"; sleep 1; xdotool key --window "$WINID" a'
wait
```
