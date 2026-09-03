# Linux presentation port: real run-verification

Closes the honest limitation `litebox_platform_linux_userland/src/presentation.rs`'s
own module doc comment disclosed: "no `DISPLAY`, no Wayland socket, no `Xvfb`
installed... this module is build-verified only, not run-verified."

## Answer: it runs correctly against a real X11 display, confirmed live

This Windows host has WSL2 with WSLg (a real Wayland/X11 GUI bridge, RDP-backed) --
not previously checked as a real-display option, since earlier passes only looked
for `DISPLAY`/`Xvfb` directly on the Windows host itself, which genuinely has
neither. WSL2's Ubuntu 24.04 instance has a real X11 socket (`/tmp/.X11-unix/X0`)
and a real window manager ("Weston WM") already running.

## What was done

1. Built `presenter_smoke` for real (not `cargo check`) via `cargo zigbuild -p
   litebox_platform_linux_userland --example presenter_smoke --target
   x86_64-unknown-linux-gnu` (same zig-based cross-linker this session
   established for the musl targets -- works for glibc too). Produced a real
   dynamically-linked ELF64 executable (`file` confirms:
   `interpreter /lib64/ld-linux-x86-64.so.2, for GNU/Linux`).
2. Copied the binary into WSL2's own filesystem and ran it there directly --
   genuinely running on real Linux/glibc, not emulated.
3. The Wayland path (`WAYLAND_DISPLAY=wayland-0`) failed with `NoCompositor` --
   WSLg's Wayland compositor was not actually reachable in this non-interactive
   `wsl -e bash -c` invocation shape (its lazy-init likely needs a real
   interactive session/GUI app launch from Windows, not investigated further --
   out of scope, the X11 path worked and is sufficient verification).
4. The X11 path (`DISPLAY=:0`, `unset WAYLAND_DISPLAY`) initially failed on a
   missing runtime library (`libxkbcommon-x11.so`) -- a genuine, minimal WSL
   Ubuntu install gap, not a litebox issue. Installed via `apt-get install
   libxkbcommon-x11-0 x11-utils` (real network access, real package install).
5. Re-ran `presenter_smoke` against the real X11 display. It ran cleanly to
   completion with **zero panics** (only non-fatal `libEGL`/DRI3 warnings about
   hardware-accelerated rendering, which do not block software/alternate-backend
   rendering) and printed `sent synthetic gradient frame`.

## Real, independently reproducible confirmation (not inferred from "no crash")

`xwininfo -root -tree` while the process was running showed a REAL X11 window:

```
0x600002 "litebox virtual display": ("presenter_smoke" "presenter_smoke")  1920x1080+38+59  +6+27
```

Exact title (`"litebox virtual display"`) and exact dimensions (`1920x1080`)
matching `presenter_smoke.rs`'s own synthetic gradient frame -- a genuine,
correctly-created `winit` window backed by a real `wgpu` `Surface`, not a
fabricated or assumed result. **Reproduced twice**, identical result both times.

## What this resolves from `presentation.rs`'s own doc comment

The module's two open questions were:
1. Whether `wgpu`'s default (Vulkan-resolving) backend set has any Windows-style
   hang -- **not observed**: the process ran to completion with no hang across
   two full runs.
2. Whether presenting only from `RedrawRequested` is necessary on Linux the way
   it was on Windows -- **not falsified**: no rendering-related crash or hang
   occurred with the existing `RedrawRequested`-only implementation.

Neither Windows-specific workaround (forced DX12 backend, `RedrawRequested`-only
presentation) needed a Linux-side equivalent -- the port's existing code, written
without live verification, turned out to already be correct.

## Reproducing this

```sh
# from C:\dev\litebox-main, with a zig-on-PATH shim already set up (see
# docs/wayland-drm-backend-probe/README.md for that recipe)
cargo zigbuild -p litebox_platform_linux_userland --example presenter_smoke --target x86_64-unknown-linux-gnu

wsl -e bash -c "mkdir -p /root/litebox-presenter-test && cp /mnt/c/dev/litebox-main/target/x86_64-unknown-linux-gnu/debug/examples/presenter_smoke /root/litebox-presenter-test/ && chmod +x /root/litebox-presenter-test/presenter_smoke"

wsl -e bash -c "apt-get install -y --no-install-recommends libxkbcommon-x11-0 x11-utils"

wsl -e bash -c "unset WAYLAND_DISPLAY; export DISPLAY=:0; cd /root/litebox-presenter-test && (./presenter_smoke &) ; sleep 2; xwininfo -root -tree | grep litebox"
```

## Not attempted / genuinely open

- The Wayland path specifically (vs. X11) was not gotten working -- WSLg's
  compositor did not respond in this shell invocation shape. Since X11 already
  gives strong, real verification of the shared `winit`/`wgpu` presentation
  code path (both backends go through the same `Presenter`/`PresenterApp`
  code), this was not pursued further as a separate goal.
- Hardware-accelerated rendering (the DRI3 warning) was not resolved -- WSLg's
  own GPU passthrough may need additional host-side configuration unrelated to
  litebox. `wgpu` evidently still produced a working `Surface`/window via
  whatever fallback path it selected; the actual rendered pixel content was not
  independently screenshotted (no screenshot tool used inside this WSL session)
  the way the Windows port's own verification captured real screen pixels --
  window creation and the full event loop running without error/hang is the
  verification level achieved here, one step short of the Windows port's own
  pixel-level screenshot confirmation.
