# Webtop guest rebuilt from host packages; rungs 1-4 status (2026-10-12)

The container was recreated on 2026-10-11 and `/tmp/lbx` (rootfs tarballs, harness, runner copy) was
lost. The old `rootfs-x11.tar` seed does not survive. This is the rebuild that replaced it.

## Rebuild recipe (no network, no debootstrap)

The host already has the webtop packages installed, so the guest is assembled from them:

1. Binaries + library closure: `Xvfb xfwm4 xfce4-session xfce4-panel xfdesktop xfsettingsd xclock xwd
   xdpyinfo xwininfo dbus-daemon dbus-launch glycin-thumbnailer` + chromium + dash/base64/coreutils,
   and `ldd` of all of them (plus `xfce4/`, `gdk-pixbuf-2.0/`, `gio/modules/` ELF files).
2. Map files to packages with `dpkg -S`, then take `dpkg-query -L` for those packages. Drop
   `/usr/share/{doc,man,lintian,info}`, `/usr/share/locale` except en, `gcc`, `cpp`, `include`.
   Result: ~2.2 GB. Copy with `cp -a --parents` (keeps symlinks), then resolve dangling links from the host.
3. Add explicitly: `/usr/share/{fonts,X11,icons,themes,glib-2.0,xfce4,dbus-1,mime,applications,
   glycin-loaders,gtk-3.0,gdk-pixbuf-2.0}`, `/etc/{fonts,xdg}`, `/usr/libexec/glycin-loaders`,
   `/usr/lib/chromium` (minus `extensions/`), `/usr/lib/xorg/modules` (from `xserver-xorg-core`),
   `xkbcomp` (`x11-xkb-utils`), `xkb-data`, the Mesa DRI dir `/usr/lib/x86_64-linux-gnu/dri` (48 MB,
   from `libgl1-mesa-dri`) with its ldd closure, and `/usr/share/drirc.d`.
4. Guest basics: `/etc/{passwd,group,hosts,nsswitch.conf,resolv.conf,localtime,machine-id}`,
   `/opt/lib` (the host `libudev` is a FAKE stub: ship `libudev.so.1` -> `libudev.so.1.0.0-fake`),
   `/tmp`, `/tmp/home`, `/tmp/xdg`, `/tmp/.X11-unix` all 1777, `/bin/sh -> /usr/bin/dash`.
5. Coreutils the boot script needs: `cat ls rm head tr wc mkdir sleep base64 chmod grep awk`
   (+ closures). `awk` is `/etc/alternatives/awk` -> mawk: copy the resolved file.
   **`timeout` and `cut` are NOT safe to rely on**: `timeout` returned 0-byte captures; `cut` was missing.
6. `chmod -R go+rX` drops the sticky/world-write bits on `/tmp*` — re-apply `chmod 1777` last.
7. Tar with `--owner=0 --group=0 --numeric-owner`. Check for dangling links (0 expected).

Script: `docs/tools/webtop-wt.sh` (rung 1-4, `<rung> <hold> [chromium flags]`, watchdogged X clients).
Decode: `docs/tools/xwdhist.py <out-file>` (handles 24/32 bpp, `XWD`/`CXWD`/`PXWD` blocks).
Run it with the gated wrapper: `PROG="/bin/sh /wt.sh" TAR=<wt.tar> RUNNER=<lbxrun> run-wt.sh <tag> <secs> <rung> <hold> [flags]`
(one runner at a time; sweep orphans by argv — `timeout -s KILL` leaves fork children alive).

## Results

| rung | result |
|---|---|
| 1 Xvfb | PASS. root captures 3,148,907 B; decoded 2 colours (589,824 black + 196,608 white) = root weave |
| 2 +xfwm4 | root unchanged (no clients), xfwm4 window present |
| 3 +xfce4-session | PASS for the desktop: root 201 colours incl. the panel bar and desktop grey `cececeff` |
| 4 +chromium | chromium window `litebox chromium - Chromium` mapped 1024x744+0+24 in the xfwm4 frame; grabs are uniform `cececeff` = desktop grey showing through: **MAPPED, NEVER DRAWN** |

Variants at rung 4 (all: window mapped, never drawn):
- default flags: GPU process exits: `ANGLE ... GLX is not present` (Xvfb without GLX).
- `+extension GLX` (Xvfb): `glXQueryExtensionsString returned NULL`; then the X server stops answering `xwd`.
- `--use-angle=swiftshader --enable-unsafe-swiftshader`: stalled before the first capture (not diagnosed).
- `--disable-gpu`: no GPU-process errors, no X11/compositor error in chromium stderr; still never drawn.
- `--disable-gpu` + `-extension MIT-SHM` on Xvfb (XPutImage only): still never drawn. **MIT-SHM is not the cause.**

## litebox defects found and fixed this session

- `5d765ec7` a refused guest `mremap` aborted the host process (`remap_pages` `expect`). Now an errno.
- `bef9d55a` fresh memfd objects take the pool token, not a process-local counter name (silent alias).
  Switch `LITEBOX_MEMFD_TOKEN_OFF`. One ON probe aborted with `shared heap lock stuck`; not reproduced on repeat.

## Open

- `mmap`'s `_ => panic!("unhandled mmap error")` (`litebox_platform_linux_userland/src/lib.rs` ~2908) is the
  same guest-reachable class as the mremap panic. Not changed.
- `pool_table` = 128 slots, never freed (`shared_heap.rs:47`): `shmat` ENOMEM after 128 attached segments.
- Headless `--headless=new` in the minimal (non-webtop) rootfs: no renderer starts, no DOM in 240 s.
- The paint question is now a renderer-to-viz frame problem: the window is mapped, the compositor has
  nothing to show. Next: whether the renderer commits frames at all (Mojo/BeginFrame, timerfd firing in
  the renderer and GPU-less viz), not the X server.

## Paint discriminators (2026-10-12, later runs)

Single-process `--single-process --disable-gpu` in this webtop **PAINTS**: chromium window 910-1113 colours,
BGRA `00d4ff` = RGB `ffd400` = the page's yellow. Multi-process with the same flags does not.
So the blocker is the cross-process path (renderer -> viz), not the X11 presentation.

A/B of the memfd switch (`LITEBOX_MEMFD_TOKEN_OFF`), multi-process, `--disable-gpu`, compositor vmodules:
- token path ON (default, `bef9d55a`): chromium window shows a **striped blue band** (vertical stripes,
  two colours, rows ~216 of 744), not the page. Rendered to PNG and inspected: real pixels reach the
  compositor, but the content is wrong. Not blank.
- token path OFF (`LITEBOX_MEMFD_TOKEN_OFF=1`): chromium window **blank grey** (`cececeff`, no stripes).

Reading: the token path makes frames cross processes (something is painted), but the bytes the viz
side reads are not the bytes the renderer wrote. Candidate: pool-offset mapping or stride/format in the
shared bitmap, under `map_shared_memory` (`litebox_platform_linux_userland/src/lib.rs` ~3125-3145) and
`try_memfd_mmap` (`litebox_shim_linux/src/syscalls/mm.rs:1260`). Not yet diagnosed.

Verbose chromium logging (`--v=1`) makes the guest too slow to finish: do not use it for runs that must
complete; use `--vmodule` only.

Band structure (token path ON, mid-band row y=559 of the 1024x744 chromium window): colour changes EVERY
pixel, repeating every 4 px with a 2:1:1 mix (512 px `ff7f0000`, 256 px `30403fff`, 256 px `b04c3fff`).
That is a stride or pixel-format mismatch between the producer and the reader of the same bitmap, not
random aliasing. Next: find where the viz-side reader takes its row stride/format for a shared software
bitmap, and check it against the producer's size under the pool-token mapping.
