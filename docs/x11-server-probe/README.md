# X11 server (Xorg) feasibility probe

Answers the remaining open question on PRD row `gui-x11-server-on-drm-future`:
after `docs/x11-libdrm-client-probe/` proved a real libdrm CLIENT works
end-to-end against litebox's DRM emulation (commit `fafa842`), what would it
actually take to run a real X SERVER (not just a libdrm smoke-test client)
as a guest process, providing an actual X11 display?

## Answer: a real prebuilt Xorg exists and is obtainable, but its runtime
## dependency closure is genuinely large -- multi-session-scale work, not a
## single pass. No code changes made this pass; this is a scoping result.

## What was checked, concretely (not assumed)

Alpine's `community` repo (not `main`) ships a real prebuilt `xorg-server`
package (`xorg-server-21.1.24-r0.apk`, a plain gzipped tar, same technique
used for `libdrm`/`libxkbcommon` elsewhere in this project):

```sh
curl -s -o xorg-server.apk \
  https://dl-cdn.alpinelinux.org/alpine/edge/community/x86_64/xorg-server-21.1.24-r0.apk
tar -xzf xorg-server.apk
```

Good news: it ships `usr/lib/xorg/modules/drivers/modesetting_drv.so` --
exactly the KMS/DRM driver litebox's dumb-buffer device needs, no legacy
VGA/PCI-BIOS probing driver required. The real binary is
`usr/libexec/Xorg` (1.88 MB); `usr/bin/Xorg` is a tiny wrapper script that
execs `usr/libexec/Xorg.wrap` (a 14 KB suid-privilege-drop shim) if present,
else the real binary directly -- invoking `usr/libexec/Xorg` directly as an
ordinary guest process would sidestep any suid-wrapper complexity entirely,
since litebox's guest execution has no real privilege-separation boundary to
drop from in the first place.

**The real blocker is the dependency closure**, read directly from the
package's own `.PKGINFO`, not guessed:

```
depend = font-cursor-misc
depend = font-misc-misc
depend = mesa-egl
depend = xkbcomp
depend = xkeyboard-config
depend = xorg-server-common
depend = so:libGL.so.1
depend = so:libXau.so.6
depend = so:libXdmcp.so.6
depend = so:libXfont2.so.2
depend = so:libc.musl-x86_64.so.1
depend = so:libdrm.so.2
depend = so:libepoxy.so.0
depend = so:libgbm.so.1
depend = so:libnettle.so.8
depend = so:libpciaccess.so.0
depend = so:libpixman-1.so.0
depend = so:libudev.so.1
depend = so:libxcvt.so.0
depend = so:libxshmfence.so.1
```

Every one of those in turn has its own dependency chain -- checked
`mesa-egl`'s own `.PKGINFO` as a sample: it alone depends on `mesa` itself,
`mesa-gles`, `libX11`, `libX11-xcb`, `libwayland-client`, `libexpat`, and six
separate `libxcb-*` variants. `mesa` (the package `mesa-egl` requires) is
**13.7 MB compressed** on its own -- Mesa bundles a full software GL/EGL/GLX
implementation (LLVMpipe/softpipe rasterizers), not a thin shim.

`libudev.so.1` is a real concern (talks to the kernel's real udev device
subsystem, which doesn't exist as a guest concept in litebox) -- but
`libudev-zero` (Alpine `community`, real package,
`libudev-zero-1.0.4-r0.apk`) is a genuine, much smaller libudev-API-compatible
shim built for exactly this "I need the libudev.so.1 SONAME but not a real
udev daemon" situation (used by minimal/embedded Linux distros). This
specific piece is NOT a hard blocker -- it's a small `.a`/`.so` linkable the
same way `libxkbcommon.a` and `libdrm.so.2` were fetched for the other two
probes.

## Why this wasn't attempted as a full build-and-run this pass

Extrapolating from the closure above, a real attempt would need on the order
of 15-25+ shared libraries fetched, syscall-rewritten with
`litebox_syscall_rewriter`, and correctly placed into a guest rootfs (versus
the 1-3 libraries each of `x11-libdrm-client-probe`/`wayland-drm-backend-probe`
needed) -- plus font data, `xkbcomp`'s own keymap-compilation runtime
dependency, and Mesa's own internal driver-loading behavior (which may try to
probe for a real GPU via `libpciaccess`/`libdrm` render-node enumeration in
ways this virtual device may not satisfy identically to a real one, a further
unknown not yet investigated). This is realistically several hours of
careful, incremental work (fetch one dependency, attempt to load, discover
the next missing symbol, repeat) -- multi-session-scale, matching this
project's own prior assessment of "a full Xorg build is realistically out of
scope for a single pass," now with concrete evidence backing that estimate
rather than just an assumption.

## What this DOES prove, and what remains

**Proves**: this is achievable in principle -- every dependency exists as a
real, fetchable Alpine package (no dead end found), `modesetting_drv.so`
means litebox's existing DRM ioctl surface is architecturally the right
target, and `libudev-zero` removes what looked like the hardest single
non-graphics dependency. Nothing found here contradicts this project's
existing architecture decision (a guest-side X server, not a host-side
protocol server) -- if anything it reinforces it, since `Xorg` itself is
just another ordinary ELF binary from litebox's point of view.

**Remains**: the actual incremental fetch-link-run cycle across the full
dependency closure, likely surfacing further real litebox gaps along the way
(matching the pattern of every other probe this session: `OBJ_GETPROPERTIES`,
nested epoll, etc. were each found only by actually running a real client
deep enough to hit them) -- a genuine next-session task, not attempted here
to avoid a rushed, incomplete, or fabricated "it works" claim.
